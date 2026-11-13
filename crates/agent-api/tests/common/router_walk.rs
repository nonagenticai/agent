//! Walk the router from its source (B1 T21): every `.route("/api/…", verb(handler)…)` registered under `src/`, and
//! the guard each handler's extractors name. axum cannot enumerate a built `Router`, so the registrations are read
//! from the code that builds it; tests then PROBE each route over HTTP so a parse that missed a guard still fails.
use std::path::{Path, PathBuf};

/// One registered `verb path → handler`.
#[derive(Clone, Debug)]
pub struct RouteDef {
    pub method: String,
    pub path: String,
    pub handler: String,
    pub file: String,
    /// The handler's parameter list (whitespace collapsed).
    pub params: String,
}

/// What a handler's extractors demand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Guard {
    /// `Requires<role::X>` / `JobRequires<role::X>` / `TenantAdmin`: the §R.1 role key.
    Role(String),
    /// `SuperAdmin` / `SaInTenant`.
    SuperAdmin,
    /// `ServiceClaims` (a service account; the route checks it itself).
    Service,
    /// `Member` / `JobMember` / `TenantCtx` / `Caller`: any member (an observer included) or person.
    Member,
    /// `EdgeKey`: the agent-edge internal door (`EDGE_INTERNAL_KEY`; no person, no tenant role).
    Internal,
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("read src dir") {
        let p = e.expect("dir entry").path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The public `agent-edge` binary's routes (crate::channels, src/bin): anonymous provider webhooks and web chat, never
/// mounted on agent-api. tests/channel_edge.rs pins both directions (`edge_serves_no_admin_route`,
/// `api_serves_no_channel_route`), so the role walk covers agent-api's router only.
const EDGE_ONLY: [&str; 2] = ["src/channels", "src/bin"];

/// Every `src/**/*.rs` as (path relative to the crate, whitespace-collapsed text).
pub fn sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).expect("read source");
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            (rel, squash(&text))
        })
        .collect()
}

/// Collapse every whitespace run to one space.
pub fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text from `open` (an index just past a `(`) to its matching `)`.
fn balanced(s: &str, open: usize) -> &str {
    let mut depth = 1usize;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &s[open..open + i];
                }
            }
            _ => {}
        }
    }
    &s[open..]
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `verb(handler)` calls in a method chain.
fn verbs(chain: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for verb in ["get", "post", "put", "patch", "delete"] {
        let pat = format!("{verb}(");
        let mut from = 0;
        while let Some(i) = chain[from..].find(&pat) {
            let at = from + i;
            from = at + pat.len();
            if chain[..at].chars().next_back().is_some_and(is_ident) {
                continue;
            }
            let rest = &chain[at + pat.len()..];
            // `handler` or a path `crate::routes::jobs::handler`
            let name: String = rest
                .chars()
                .take_while(|c| is_ident(*c) || *c == ':')
                .collect();
            if !name.is_empty() && rest[name.len()..].starts_with(')') {
                let last = name.rsplit("::").next().unwrap_or_default().to_string();
                out.push((verb.to_uppercase(), last));
            }
        }
    }
    out
}

/// The parameter list of `async fn handler(` in `text`.
fn params_of(text: &str, handler: &str) -> Option<String> {
    let pat = format!("async fn {handler}(");
    let at = text.find(&pat)? + pat.len();
    Some(balanced(text, at).to_string())
}

/// Every agent-api `/api/…` route registered under `src/` (not the [`EDGE_ONLY`] files).
pub fn routes() -> Vec<RouteDef> {
    let all: Vec<(String, String)> = sources()
        .into_iter()
        .filter(|(f, _)| EDGE_ONLY.iter().all(|e| !f.starts_with(e)))
        .collect();
    let mut out = Vec::new();
    for (file, text) in all.iter().cloned() {
        let mut from = 0;
        while let Some(i) = text[from..].find(".route(") {
            let open = from + i + ".route(".len();
            from = open;
            let call = balanced(&text, open);
            let call = call.trim_start();
            let Some(rest) = call.strip_prefix('"') else {
                continue;
            };
            let Some(end) = rest.find('"') else {
                continue;
            };
            let path = &rest[..end];
            if !path.starts_with("/api/") {
                continue;
            }
            for (method, handler) in verbs(&rest[end + 1..]) {
                // the handler's own file first, else the one module that defines it (a path-qualified handler)
                let params = params_of(&text, &handler)
                    .or_else(|| all.iter().find_map(|(_, t)| params_of(t, &handler)))
                    .unwrap_or_else(|| panic!("{file}: no `async fn {handler}(`"));
                out.push(RouteDef {
                    method,
                    path: path.to_string(),
                    handler,
                    file: file.clone(),
                    params,
                });
            }
        }
    }
    out
}

/// `TenantAdmin` → `tenant_admin`.
pub fn snake(camel: &str) -> String {
    let mut s = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
    }
    s
}

/// The guards a parameter list names (one per guarded extractor).
pub fn guards(params: &str) -> Vec<Guard> {
    let mut out = Vec::new();
    for piece in params.split(',') {
        let Some((_, ty)) = piece.rsplit_once(": ") else {
            continue;
        };
        let ty = ty.trim();
        let role = ty
            .strip_prefix("Requires<role::")
            .or_else(|| ty.strip_prefix("JobRequires<role::"))
            .and_then(|r| r.strip_suffix('>'));
        let g = match (role, ty) {
            (Some(r), _) => Guard::Role(snake(r)),
            (None, "TenantAdmin") => Guard::Role("tenant_admin".into()),
            // `Offboarder` (crate::tenants::offboarder): a person super_admin, or the ONE pinned service account
            // (tests/offboard_service.rs covers the service side); walked as SuperAdmin.
            (None, "SuperAdmin" | "SaInTenant" | "Offboarder") => Guard::SuperAdmin,
            (None, "ServiceClaims") => Guard::Service,
            (None, "EdgeKey") => Guard::Internal,
            // `JobCaller` (crate::interaction::apikeys): a member as `JobMember`, or the job's own API key; walked as
            // Member (the interaction routes; tests/api_keys.rs covers the key side)
            (None, "Member" | "JobMember" | "JobCaller" | "TenantCtx" | "Caller") => Guard::Member,
            _ => continue,
        };
        out.push(g);
    }
    out
}

/// A concrete URI for `path`: `{job}` → `job`, `{sub}` → `sub`, any other segment → a fresh uuid.
pub fn fill(path: &str, job: &str, sub: &str) -> String {
    path.split('/')
        .map(|seg| match seg {
            "{job}" => job.to_string(),
            "{sub}" => sub.to_string(),
            s if s.starts_with('{') => uuid::Uuid::new_v4().to_string(),
            s => s.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}
