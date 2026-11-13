# /// script
# requires-python = ">=3.12"
# dependencies = ["psycopg[binary]>=3.3"]
# ///
"""Check every literal `sqlx::query*` SQL string against the migrated schema, without compiling.

The crates build SQL at runtime (`sqlx::query("...")`, never the `query!` macros), so a wrong table, column,
type or syntax only surfaces when a database test runs it. This applies each crate's migrations to a scratch
database and asks Postgres to `PREPARE` every literal statement: parse + name resolution + type checking,
no execution.

Not checked (counted in the summary): non-literal SQL (`format!`, variables), DDL/utility statements, and
multi-statement strings — PREPARE accepts only one SELECT/INSERT/UPDATE/DELETE/MERGE/VALUES/WITH statement.
A statement whose only problem is an untyped parameter (`$1` with no inferable type) passes.

Known, intended failures (a test asserting an error, a test-local table) go in sqlcheck-allow.txt as
`<path>\t<sha1 of the decoded SQL>` lines, which survive line moves. `--update-allowlist` rewrites it.

Database: AGENT_SQLCHECK_URL (default from ~/.config/agent-sqlcheck/env) names the agent-api scratch DB; the
Never point it at a database holding data: each
run whose migration set changed drops and recreates the `public` schema.

Usage: uv run scripts/static/sqlcheck.py [--update-allowlist] [paths...]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import psycopg

ROOT = Path(__file__).resolve().parents[2]
ALLOW = Path(__file__).with_name("sqlcheck-allow.txt")
CRATES = {  # crate dir -> (migrations dir, database suffix)
    "crates/agent-api": ("crates/agent-api/migrations", ""),
    "crates/agent-model": ("crates/agent-api/migrations", ""),
}
PATTERNS = [
    "sqlx::query($S)",
    "sqlx::query_as($S)",
    "sqlx::query_as::<$$$T>($S)",
    "sqlx::query_scalar($S)",
    "sqlx::query_scalar::<$$$T>($S)",
]
PREPARABLE = re.compile(r"^\s*(\(\s*)*(SELECT|INSERT|UPDATE|DELETE|MERGE|VALUES|WITH|TABLE)\b", re.I)
PARAM = re.compile(r"\$\d+")
# Type errors Postgres raises only because PREPARE infers an untyped `$n` from context (`$2 - interval '7 days'`
# deduces interval) while sqlx sends typed binds: with parameters present these are warnings, not failures.
# Undefined table/column (42P01/42703), syntax (42601) and everything else stay failures.
PARAM_TYPE_STATES = {"42P18", "42P08", "42804", "42883", "42846", "42725"}
# sqlx's own bookkeeping table (sqlx-postgres migrate.rs); 0001 REVOKEs on it, so it must exist first.
SQLX_TABLE = """CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
    success BOOLEAN NOT NULL, checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL)"""
ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "\\": "\\", "0": "\0", "'": "'", '"': '"'}


def database_url() -> str:
    url = os.environ.get("AGENT_SQLCHECK_URL")
    if not url:
        env = Path.home() / ".config/agent-sqlcheck/env"
        if env.is_file():
            for line in env.read_text().splitlines():
                if line.startswith("AGENT_SQLCHECK_URL="):
                    url = line.split("=", 1)[1].strip()
    if not url:
        sys.exit("sqlcheck: set AGENT_SQLCHECK_URL (a scratch database; its public schema is recreated)")
    return url


def decode_literal(text: str) -> str | None:
    """A Rust string literal's value, or None for anything that is not a single plain/raw string literal."""
    m = re.fullmatch(r'r(#*)"(.*)"\1', text, re.S)
    if m:
        return m.group(2)
    if not (text.startswith('"') and text.endswith('"')) or len(text) < 2:
        return None
    body, out, i = text[1:-1], [], 0
    while i < len(body):
        c = body[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        nxt = body[i + 1]
        if nxt == "\n":  # line continuation: drop the newline and the next line's leading whitespace
            i += 2
            while i < len(body) and body[i] in " \t\n\r":
                i += 1
        elif nxt == "u":
            end = body.index("}", i)
            out.append(chr(int(body[i + 3 : end], 16)))
            i = end + 1
        elif nxt == "x":
            out.append(chr(int(body[i + 2 : i + 4], 16)))
            i += 4
        else:
            out.append(ESCAPES.get(nxt, nxt))
            i += 2
    return "".join(out)


def extract(paths: list[str]) -> list[dict]:
    found = {}
    for pattern in PATTERNS:
        out = subprocess.run(
            ["ast-grep", "run", "-l", "rust", "-p", pattern, "--json=stream", *paths],
            cwd=ROOT, capture_output=True, text=True, check=True,
        ).stdout
        for line in out.splitlines():
            m = json.loads(line)
            s = m["metaVariables"]["single"]["S"]
            key = (m["file"], s["range"]["byteOffset"]["start"])
            found[key] = {"file": m["file"], "line": s["range"]["start"]["line"] + 1, "text": s["text"]}
    return sorted(found.values(), key=lambda q: (q["file"], q["line"]))


def migrate(conn: psycopg.Connection, migrations: Path) -> None:
    files = sorted(migrations.glob("*.sql"))
    digest = hashlib.sha256(b"".join(f.name.encode() + f.read_bytes() for f in files)).hexdigest()
    have = conn.execute(
        "SELECT obj_description('public'::regnamespace, 'pg_namespace')"
    ).fetchone()[0]
    if have == digest:
        return
    conn.execute("DROP SCHEMA public CASCADE")
    conn.execute("CREATE SCHEMA public")
    conn.execute("GRANT USAGE ON SCHEMA public TO PUBLIC")
    conn.execute(SQLX_TABLE)
    for f in files:
        try:
            with conn.transaction():
                conn.execute(f.read_text())
        except psycopg.Error as e:
            sys.exit(f"sqlcheck: migration {f.relative_to(ROOT)} failed: {e}")
    conn.execute(f"COMMENT ON SCHEMA public IS '{digest}'")


def check(conn: psycopg.Connection, sql: str) -> tuple[str, str] | None:
    """None when the statement prepares; else ("error"|"warning", message)."""
    try:
        with conn.transaction():
            conn.execute(f"PREPARE sqlcheck_stmt AS {sql}")
            conn.execute("DEALLOCATE sqlcheck_stmt")
    except psycopg.Error as e:
        msg = (e.diag.message_primary or str(e)).strip()
        if e.sqlstate in PARAM_TYPE_STATES and PARAM.search(sql):
            return ("warning", msg)
        return ("error", msg)
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--update-allowlist", action="store_true", help="record every current failure as known")
    ap.add_argument("--warnings", action="store_true", help="list parameter-type warnings too")
    ap.add_argument("paths", nargs="*", default=["crates"])
    args = ap.parse_args()

    base = database_url()
    allow = set()
    if ALLOW.is_file():
        allow = {l for l in ALLOW.read_text().splitlines() if l and not l.startswith("#")}

    queries = extract(args.paths)
    conns: dict[str, psycopg.Connection] = {}
    stats = {"checked": 0, "non_literal": 0, "not_preparable": 0, "allowed": 0}
    failures, warnings, current = [], [], set()
    for q in queries:
        crate = next((c for c in CRATES if q["file"].startswith(c + "/")), None)
        if crate is None:
            continue
        sql = decode_literal(q["text"])
        if sql is None:
            stats["non_literal"] += 1
            continue
        body = sql.strip().rstrip(";")
        if not PREPARABLE.match(body) or ";" in body.replace("';'", ""):
            stats["not_preparable"] += 1
            continue
        migrations, suffix = CRATES[crate]
        if suffix not in conns:
            conn = psycopg.connect(base + suffix, autocommit=True)
            conn.execute("SELECT pg_advisory_lock(hashtext('agent-sqlcheck'))")
            migrate(conn, ROOT / migrations)
            conns[suffix] = conn
        stats["checked"] += 1
        res = check(conns[suffix], body)
        if res is None:
            continue
        level, err = res
        if level == "warning":
            warnings.append((q, err))
            continue
        key = f"{q['file']}\t{hashlib.sha1(sql.encode()).hexdigest()}"
        current.add(key)
        if key in allow:
            stats["allowed"] += 1
        else:
            failures.append((q, err))
    for c in conns.values():
        c.close()

    if args.update_allowlist:
        ALLOW.write_text(
            "# sqlcheck known failures: <path>\\t<sha1 of decoded SQL>. Regenerate with --update-allowlist.\n"
            + "".join(f"{k}\n" for k in sorted(current))
        )
        print(f"sqlcheck: wrote {len(current)} allowlist entries to {ALLOW.relative_to(ROOT)}")
        return 0
    if args.warnings:
        for q, err in warnings:
            print(f"{q['file']}:{q['line']}: warning (untyped parameter): {err}")
    for q, err in failures:
        print(f"{q['file']}:{q['line']}: {err}")
    print(
        f"sqlcheck: {stats['checked']} prepared, {len(failures)} failed, {stats['allowed']} allowlisted, "
        f"{len(warnings)} parameter-type warnings{'' if args.warnings else ' (--warnings lists them)'}; "
        f"skipped {stats['non_literal']} non-literal, {stats['not_preparable']} DDL/multi-statement",
        file=sys.stderr,
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
