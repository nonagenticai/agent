//! B2 T16 anti-drift (spec B2 §2, D54): a design working row is written ONLY by its kind's target (`targets.rs`) or
//! inside the closure a handler passes to `design::track` / `design::track_tenant`. Any other `INSERT` / `UPDATE` /
//! `DELETE` / `TRUNCATE` on a registered working table in `src/` would change a job's design without a ledger row,
//! a `draft_rev` bump or an audit event, and fails [`no_design_table_written_outside_track`].
//!
//! A write is accepted when it is
//! 1. in the owning kind's targets module ([`DesignTable::targets`]): `get`/`set` are the design layer itself (apply,
//!    rollback, discard and follow-live restore working rows through `set`);
//! 2. lexically inside the argument list of a `track(` / `track_tenant(` call (an inline closure);
//! 3. inside a `fn` whose name is called inside such an argument list in the same file (`move |c| Box::pin(f(c, ..))`);
//! 4. listed in [`ALLOWED`] with its reason (each entry must still match a write, else the test fails as stale).
//!
//! Grep-based, no database: SQL is matched on identifier tokens, so a statement split over `\`-continued lines or a
//! raw string is still seen; `//` comment lines are ignored. Each later plan appends its tables to [`DESIGN_TABLES`].
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// One design working table (spec B2 §2.5 "working rows", §3).
struct DesignTable {
    table: &'static str,
    /// Only writes that set this column count (`jobs` is shared: only `UPDATE jobs SET languages` is a design write;
    /// creating or deleting a job is not).
    column: Option<&'static str>,
    kinds: &'static [&'static str],
    /// The owning kind's targets module, relative to the crate root.
    targets: &'static str,
}

/// B2's working tables. Not listed, on purpose: `job_target_history` / `business_rule_history` (append-only logs
/// written next to a tracked write, not a design value); `draft_ledger`, `release_*`, `published_change`,
/// `release_conflict` (the ledger/release machinery itself, owned by `design::` and `release::`).
const DESIGN_TABLES: &[DesignTable] = &[
    DesignTable {
        table: "jobs",
        column: Some("languages"),
        kinds: &["languages"],
        targets: "src/release/targets.rs",
    },
    DesignTable {
        table: "job_setting_override",
        column: None,
        kinds: &["setting"],
        targets: "src/release/targets.rs",
    },
    DesignTable {
        table: "job_overlay_row",
        column: None,
        kinds: &["overlay"],
        targets: "src/release/targets.rs",
    },
    DesignTable {
        table: "business_rule",
        column: None,
        kinds: &["rule_values", "rule_passage"],
        targets: "src/release/targets.rs",
    },
    DesignTable {
        table: "job_target",
        column: None,
        kinds: &["selection"],
        targets: "src/release/targets.rs",
    },
    // B3 T3 (D69): the tenant-wide use bindings
    DesignTable {
        table: "llm_use_binding",
        column: None,
        kinds: &["llm_use"],
        targets: "src/providers/targets.rs",
    },
    DesignTable {
        table: "provider_slot",
        column: None,
        kinds: &["provider_slot"],
        targets: "src/providers/targets.rs",
    },
    // B3 T10 (D68, R10): knowledge-kind bindings
    DesignTable {
        table: "job_knowledge_kind",
        column: None,
        kinds: &["knowledge"],
        targets: "src/knowledge/kinds.rs",
    },
    // B3 T4a (D83/D93): a job's generation pin per root store
    DesignTable {
        table: "job_embedding_pin",
        column: None,
        kinds: &["embedding_pin"],
        targets: "src/knowledge/targets.rs",
    },
    // B7 (D54/D69): routes per job, mapping overrides and tenant templates tenant-wide
    DesignTable {
        table: "operation_route",
        column: None,
        kinds: &["route"],
        targets: "src/connectors/places.rs",
    },
    DesignTable {
        table: "field_mapping_override",
        column: None,
        kinds: &["mapping_override"],
        targets: "src/connectors/places.rs",
    },
    DesignTable {
        table: "tenant_connector_template",
        column: None,
        kinds: &["tenant_template"],
        targets: "src/connectors/places.rs",
    },
    DesignTable {
        table: "tenant_connector_template_op",
        column: None,
        kinds: &["tenant_template"],
        targets: "src/connectors/places.rs",
    },
    // B4 T16 (D64): the job's handover SLA policy
    DesignTable {
        table: "handover_policy",
        column: None,
        kinds: &["handover_policy"],
        targets: "src/handover/policy.rs",
    },
    // B4 T16a (F-14, D69): the tenant holiday calendar, tenant-wide
    DesignTable {
        table: "tenant_calendar",
        column: None,
        kinds: &["tenant_calendar"],
        targets: "src/handover/calendar.rs",
    },
    // B5 T5 (D54/D62): the job's ontology, procedure versions, gate refinements / questions and toolset narrowings
    DesignTable {
        table: "ontology_entity",
        column: None,
        kinds: &[
            "entity",
            "entity_hidden",
            "entity_synonyms",
            "entity_attributes",
        ],
        targets: "src/job_design/targets/ontology.rs",
    },
    DesignTable {
        table: "ontology_entity_refinement",
        column: None,
        kinds: &["entity", "entity_hidden", "entity_synonyms"],
        targets: "src/job_design/targets/ontology.rs",
    },
    DesignTable {
        table: "ontology_relation",
        column: None,
        kinds: &["relation"],
        targets: "src/job_design/targets/ontology.rs",
    },
    DesignTable {
        table: "procedure_version",
        column: None,
        kinds: &["procedure"],
        targets: "src/job_design/targets/procedures.rs",
    },
    DesignTable {
        table: "job_gate_option_refinement",
        column: None,
        kinds: &["gate"],
        targets: "src/job_design/targets/gates.rs",
    },
    DesignTable {
        table: "job_gate_rule_refinement",
        column: None,
        kinds: &["gate"],
        targets: "src/job_design/targets/gates.rs",
    },
    DesignTable {
        table: "job_gate_question",
        column: None,
        kinds: &["gate_question"],
        targets: "src/job_design/targets/gates.rs",
    },
    DesignTable {
        table: "toolset_narrowing",
        column: None,
        kinds: &["narrowing"],
        targets: "src/job_design/targets/toolsets.rs",
    },
    DesignTable {
        table: "job_operation",
        column: None,
        kinds: &["job_operation"],
        targets: "src/job_design/targets/job_ops.rs",
    },
];

/// Legitimate untracked writers: `(file, table, reason)`.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "src/design/hooks.rs",
        "job_target",
        "on_job_created (§2.7): the job's born row (template default selection, ON CONFLICT DO NOTHING) inserted \
         before the baseline is taken — the job's first live value, not a draft change",
    ),
    (
        "src/connectors/tenant_templates.rs",
        "tenant_connector_template",
        "set_results (B7 T11): the result columns `validation` / `contract` of a validate or contract-test run — \
         results, not design (outside the tenant_template value), so no ledger row",
    ),
    (
        "src/job_design/targets/ontology.rs",
        "job_overlay_row",
        "the space_required target (B5 R4): a projection of the overlay place {object: context_space, key}; its \
         set (discard / restore) writes the `required` slots of that row back, as every target's set does",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Write {
    file: String,
    line: usize,
    op: &'static str,
    table: &'static str,
    /// Why it is accepted; `None` = a violation.
    by: Option<String>,
}

/// Identifier tokens `(text, byte offset)`; `//` comment lines are skipped.
fn tokens(src: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut off = 0;
    for line in src.split_inclusive('\n') {
        if !line.trim_start().starts_with("//") {
            let mut start = None;
            for (i, ch) in line.char_indices() {
                let ident = ch.is_ascii_alphanumeric() || ch == '_';
                match (ident, start) {
                    (true, None) => start = Some(i),
                    (false, Some(s)) => {
                        out.push((line[s..i].to_string(), off + s));
                        start = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = start {
                out.push((line[s..].to_string(), off + s));
            }
        }
        off += line.len();
    }
    out
}

/// The byte ranges of every `track(` / `track_tenant(` call's argument list (`fn track` definitions excluded).
fn track_spans(src: &str, toks: &[(String, usize)]) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    for (i, (t, off)) in toks.iter().enumerate() {
        if (t != "track" && t != "track_tenant") || (i > 0 && toks[i - 1].0 == "fn") {
            continue;
        }
        let mut p = off + t.len();
        // optional turbofish `::<…>`
        if src[p..].starts_with("::<") {
            let mut depth = 0i32;
            while p < b.len() {
                match b[p] {
                    b'<' => depth += 1,
                    b'>' => {
                        depth -= 1;
                        if depth == 0 {
                            p += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                p += 1;
            }
        }
        if p >= b.len() || b[p] != b'(' {
            continue;
        }
        if let Some(end) = close_paren(b, p) {
            out.push((p, end));
        }
    }
    out
}

/// The matching `)` of the `(` at `open`, skipping string literals (plain and raw).
fn close_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut p = open;
    while p < b.len() {
        match b[p] {
            b'r' if b.get(p + 1) == Some(&b'#')
                || (b.get(p + 1) == Some(&b'"') && !ident_before(b, p)) =>
            {
                let mut q = p + 1;
                let mut hashes = 0;
                while b.get(q) == Some(&b'#') {
                    hashes += 1;
                    q += 1;
                }
                if b.get(q) == Some(&b'"') {
                    let close: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    q += 1;
                    while q < b.len() && !b[q..].starts_with(&close) {
                        q += 1;
                    }
                    p = q + close.len();
                    continue;
                }
            }
            b'"' => {
                p += 1;
                while p < b.len() && b[p] != b'"' {
                    if b[p] == b'\\' {
                        p += 1;
                    }
                    p += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(p);
                }
            }
            _ => {}
        }
        p += 1;
    }
    None
}

fn ident_before(b: &[u8], p: usize) -> bool {
    p > 0 && (b[p - 1].is_ascii_alphanumeric() || b[p - 1] == b'_')
}

/// The name of the `fn` enclosing byte `off` (the nearest `fn <name>` before it).
fn enclosing_fn(toks: &[(String, usize)], off: usize) -> Option<&str> {
    let i = toks.iter().rposition(|(_, o)| *o < off)?;
    (0..=i)
        .rev()
        .find(|&j| toks[j].0 == "fn" && j + 1 < toks.len())
        .map(|j| toks[j + 1].0.as_str())
}

/// Every write to a [`DESIGN_TABLES`] table in `src` (file `rel`), classified.
fn scan(rel: &str, src: &str) -> Vec<Write> {
    let toks = tokens(src);
    let spans = track_spans(src, &toks);
    // SQL is case-insensitive: match it on lowercased tokens
    let lower: Vec<String> = toks.iter().map(|(t, _)| t.to_ascii_lowercase()).collect();
    let line_of = |off: usize| src[..off].matches('\n').count() + 1;
    let tok = |i: usize| lower.get(i).map_or("", String::as_str);
    let mut out = Vec::new();
    for (i, (_, off)) in toks.iter().enumerate() {
        let off = *off;
        let (op, mut at) = match tok(i) {
            "insert" if tok(i + 1) == "into" => ("INSERT", i + 2),
            "delete" if tok(i + 1) == "from" => ("DELETE", i + 2),
            "update" => ("UPDATE", i + 1),
            "truncate" => ("TRUNCATE", i + 1),
            _ => continue,
        };
        while matches!(tok(at), "only" | "table" | "public") {
            at += 1;
        }
        let Some(d) = DESIGN_TABLES.iter().find(|d| d.table == tok(at)) else {
            continue;
        };
        if let Some(col) = d.column {
            let sets = op == "UPDATE"
                && tok(at + 1) == "set"
                && (at + 2..at + 40)
                    .map(tok)
                    .take_while(|t| !matches!(*t, "where" | "returning" | "from" | ""))
                    .any(|t| t == col);
            if !sets {
                continue;
            }
        }
        let mut w = Write {
            file: rel.to_string(),
            line: line_of(off),
            op,
            table: d.table,
            by: None,
        };
        if rel == d.targets {
            w.by = Some(format!("the {:?} target", d.kinds));
        } else if spans.iter().any(|&(a, z)| a < off && off < z) {
            w.by = Some("inside a track call".to_string());
        } else if let Some(f) = enclosing_fn(&toks, off) {
            let call = format!("{f}(");
            if spans.iter().any(|&(a, z)| src[a..=z].contains(&call)) {
                w.by = Some(format!("fn {f}, the mutate of a track call"));
            }
        }
        if w.by.is_none()
            && let Some((_, _, why)) = ALLOWED.iter().find(|(f, t, _)| *f == rel && *t == d.table)
        {
            w.by = Some(format!("allowed: {why}"));
        }
        out.push(w);
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let p = entry.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_design_table_written_outside_track() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    assert!(files.len() > 20, "src/ was not found: {files:?}");
    let mut all = Vec::new();
    for p in &files {
        let rel = p
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let src = fs::read_to_string(p).unwrap();
        all.extend(scan(&rel, &src));
    }

    let bad: Vec<String> = all
        .iter()
        .filter(|w| w.by.is_none())
        .map(|w| format!("{}:{} {} {}", w.file, w.line, w.op, w.table))
        .collect();
    assert!(
        bad.is_empty(),
        "design working rows written outside the kind's target / a design::track closure (D54: every design \
         write records a ledger row). Route the write through design::track, or add it to ALLOWED with a reason:\n{}",
        bad.join("\n")
    );

    // non-vacuous: every table is still written by its own target (a renamed table or a broken scanner fails here)
    for d in DESIGN_TABLES {
        assert!(
            all.iter()
                .any(|w| w.table == d.table && w.file == d.targets),
            "no write to {} found in {}: the scan is blind (renamed table or moved target?)",
            d.table,
            d.targets
        );
    }
    // no stale allow-list entry
    let used: BTreeSet<(&str, &str)> = all
        .iter()
        .filter(|w| w.by.as_deref().is_some_and(|b| b.starts_with("allowed:")))
        .map(|w| (w.file.as_str(), w.table))
        .collect();
    for (f, t, _) in ALLOWED {
        assert!(
            used.contains(&(*f, *t)),
            "ALLOWED entry ({f}, {t}) matches no write: remove it"
        );
    }
}

// ---- the scanner can fail ------------------------------------------------------------------------------------------

#[test]
fn scanner_flags_an_untracked_write() {
    let src = r#"
async fn sneaky(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query("UPDATE job_target SET selection = NULL WHERE job_id = $1")
        .bind(job)
        .execute(&mut *c)
        .await?;
    sqlx::query(
        "INSERT INTO \
         job_overlay_row (job_id, object, key) VALUES ($1, 'activity', 'x')",
    );
    sqlx::query("UPDATE jobs SET languages = $2 WHERE id = $1");
    sqlx::query(r"DELETE FROM business_rule WHERE job_id = $1");
    Ok(())
}
"#;
    let ws = scan("src/routes/sneaky.rs", src);
    let ops: Vec<(&str, &str)> = ws.iter().map(|w| (w.op, w.table)).collect();
    assert_eq!(
        ops,
        [
            ("UPDATE", "job_target"),
            ("INSERT", "job_overlay_row"),
            ("UPDATE", "jobs"),
            ("DELETE", "business_rule"),
        ]
    );
    assert!(ws.iter().all(|w| w.by.is_none()), "{ws:#?}");
}

#[test]
fn scanner_accepts_track_closures_and_ignores_non_design_writes() {
    let src = r#"
// UPDATE job_target SET selection = NULL   (a comment is not a write)
async fn write_rule(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query("DELETE FROM business_rule WHERE job_id = $1 AND rule_id = $2").execute(&mut *c).await?;
    Ok(())
}
async fn handler(tx: &mut PgConnection) -> ApiResult<()> {
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query("UPDATE job_target SET selection = $2 WHERE job_id = $1 (x)").execute(c).await?;
            Ok(())
        })
    })
    .await?;
    design::track::<(), _>(&mut *c, reg, j, w, move |c| Box::pin(write_rule(c, job_id))).await?;
    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1");
    sqlx::query("INSERT INTO business_rule_history (job_id) VALUES ($1)");
    sqlx::query("INSERT INTO x VALUES ($1) ON CONFLICT DO UPDATE SET languages = 1");
    Ok(())
}
"#;
    let ws = scan("src/routes/fine.rs", src);
    assert_eq!(ws.len(), 2, "{ws:#?}");
    assert!(ws.iter().all(|w| w.by.is_some()), "{ws:#?}");
}
