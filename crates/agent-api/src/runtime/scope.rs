//! A scope is one (tenant, job, env): the unit the bus, the agents and the console are partitioned by (B4 R5).
//! Ported from care-bus-agents@96c608c `crates/carebus/src/scope.rs`, with the environment added.
//!
//! Text form `<tenant-uuid>/<job-key>/<env>` (the registry member and the nudge payload). Key prefix:
//! `{root}s:{tenant}:{job}:` for `prod` and `{root}s:{tenant}:{job}:test:` for `test`. The test prefix extends the
//! prod one, so a prod key whose own name started with `test:` would land in the test key space: [`Scope::key`]
//! refuses such a name (`test_scope_keys_never_collide_with_prod`).
//!
//! The runtime has two environment enums besides this one: `design::Env::{Production, Test}` (which design a case
//! reads, D63) and B7's `connectors::runtime::Env::{Test, Prod}` (where a connector call runs). The conversions live
//! here and only here; the database text of all three is `prod` / `test`.

use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::design;

/// The environment of a scope. `prod` serves the live design (release snapshot or baseline), `test` the draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Env {
    Prod,
    Test,
}

impl Env {
    /// Both environments, prod first: every job is served in both.
    pub const ALL: [Env; 2] = [Env::Prod, Env::Test];

    /// The database / wire text: `prod` or `test`.
    pub fn as_str(self) -> &'static str {
        match self {
            Env::Prod => "prod",
            Env::Test => "test",
        }
    }

    /// The inverse of [`Env::as_str`]; anything else is `None`.
    pub fn parse(s: &str) -> Option<Env> {
        match s {
            "prod" => Some(Env::Prod),
            "test" => Some(Env::Test),
            _ => None,
        }
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<design::Env> for Env {
    fn from(e: design::Env) -> Self {
        match e {
            design::Env::Production => Env::Prod,
            design::Env::Test => Env::Test,
        }
    }
}

impl From<Env> for design::Env {
    fn from(e: Env) -> Self {
        match e {
            Env::Prod => design::Env::Production,
            Env::Test => design::Env::Test,
        }
    }
}

impl From<Env> for crate::connectors::runtime::Env {
    fn from(e: Env) -> Self {
        match e {
            Env::Prod => crate::connectors::runtime::Env::Prod,
            Env::Test => crate::connectors::runtime::Env::Test,
        }
    }
}

impl From<crate::connectors::runtime::Env> for Env {
    fn from(e: crate::connectors::runtime::Env) -> Self {
        match e {
            crate::connectors::runtime::Env::Prod => Env::Prod,
            crate::connectors::runtime::Env::Test => Env::Test,
        }
    }
}

/// The key segment that starts every test-scope key after the shared `{root}s:{tenant}:{job}:` part.
const TEST_SEGMENT: &str = "test:";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub tenant: Uuid,
    /// The job key (`jobs.key`).
    pub job: String,
    pub env: Env,
}

impl Scope {
    /// A validated scope: a non-nil tenant and a job key matching `^[a-z][a-z0-9_]{0,62}$`.
    pub fn new(tenant: Uuid, job: impl Into<String>, env: Env) -> Result<Scope> {
        let job = job.into();
        if tenant.is_nil() {
            bail!("scope tenant is the nil uuid");
        }
        if !valid_job(&job) {
            bail!("scope job {job:?} must match ^[a-z][a-z0-9_]{{0,62}}$");
        }
        Ok(Scope { tenant, job, env })
    }

    /// `<uuid>/<job>/<env>`: a canonical (lowercase, hyphenated), non-nil tenant uuid, a job key matching
    /// `^[a-z][a-z0-9_]{0,62}$` and `prod` or `test`. The inverse of `Display`, so one scope has exactly one
    /// registry member.
    pub fn parse(s: &str) -> Result<Scope> {
        let mut parts = s.split('/');
        let (Some(tenant), Some(job), Some(env), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            bail!("scope {s:?} is not <tenant>/<job>/<env>");
        };
        let uuid =
            Uuid::parse_str(tenant).map_err(|e| anyhow::anyhow!("scope {s:?}: tenant: {e}"))?;
        if uuid.hyphenated().to_string() != tenant {
            bail!("scope {s:?}: tenant is not a lowercase hyphenated uuid");
        }
        let Some(env) = Env::parse(env) else {
            bail!("scope {s:?}: env must be prod or test");
        };
        Scope::new(uuid, job, env).map_err(|e| anyhow::anyhow!("scope {s:?}: {e}"))
    }

    /// The key prefix of this scope under the environment prefix `root` (R5).
    pub fn prefix(&self, root: &str) -> String {
        let base = format!("{root}s:{}:{}:", self.tenant.hyphenated(), self.job);
        match self.env {
            Env::Prod => base,
            Env::Test => format!("{base}{TEST_SEGMENT}"),
        }
    }

    /// `{prefix}{name}`. A prod scope refuses a `name` that starts with `test:` (it would be a key of the test
    /// scope); every key name in the runtime is a fixed word (`bus:…`, `case:…`, `deliveries:…`), so this is a
    /// programming error, never input.
    pub fn key(&self, root: &str, name: &str) -> String {
        assert!(
            self.env == Env::Test || !name.starts_with(TEST_SEGMENT),
            "a prod scope key may not start with {TEST_SEGMENT:?}: {name:?}"
        );
        format!("{}{name}", self.prefix(root))
    }

    /// The scope a key under the environment prefix `root` belongs to, or `None` for a root key (registry,
    /// heartbeats) and anything malformed. The inverse of [`Scope::key`].
    pub fn owner_of(root: &str, key: &str) -> Option<Scope> {
        let rest = key.strip_prefix(root)?.strip_prefix("s:")?;
        let (tenant, rest) = rest.split_once(':')?;
        let (job, rest) = rest.split_once(':')?;
        let tenant = Uuid::parse_str(tenant)
            .ok()
            .filter(|u| u.hyphenated().to_string() == tenant)?;
        let env = if rest.starts_with(TEST_SEGMENT) {
            Env::Test
        } else {
            Env::Prod
        };
        Scope::new(tenant, job, env).ok()
    }
}

fn valid_job(job: &str) -> bool {
    let b = job.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.tenant.hyphenated(), self.job, self.env)
    }
}

#[cfg(test)]
mod tests {
    use super::{Env, Scope};

    const T: &str = "0195f48b-d000-7000-8000-000000000099";

    #[test]
    fn connector_env_round_trips_by_text() {
        for env in Env::ALL {
            let c: crate::connectors::runtime::Env = env.into();
            assert_eq!(c.as_str(), env.as_str());
            assert_eq!(Env::from(c), env);
        }
    }

    #[test]
    fn parses_and_round_trips() {
        for env in Env::ALL {
            let s = Scope::parse(&format!("{T}/customer_care_agent/{env}")).unwrap();
            assert_eq!(s.job, "customer_care_agent");
            assert_eq!(s.tenant.to_string(), T);
            assert_eq!(s.env, env);
            assert_eq!(s.to_string(), format!("{T}/customer_care_agent/{env}"));
            assert_eq!(Scope::parse(&s.to_string()).unwrap(), s);
        }
        let longest = format!("{T}/a{}/prod", "b".repeat(62));
        assert!(Scope::parse(&longest).is_ok());
    }

    #[test]
    fn rejects_bad_scopes() {
        let too_long = format!("{T}/a{}/prod", "b".repeat(63));
        for bad in [
            "00000000-0000-0000-0000-000000000000/x/prod",
            "0195f48b-d000-7000-8000-000000000099/Bad/prod",
            "0195f48b-d000-7000-8000-000000000099",
            "0195f48b-d000-7000-8000-000000000099/x",
            "0195f48b-d000-7000-8000-000000000099/x/",
            "0195f48b-d000-7000-8000-000000000099/x/live",
            "0195f48b-d000-7000-8000-000000000099/x/prod/extra",
            "0195f48b-d000-7000-8000-000000000099//prod",
            "0195f48b-d000-7000-8000-000000000099/1job/prod",
            "0195f48b-d000-7000-8000-000000000099/a-b/test",
            "0195F48B-D000-7000-8000-000000000099/x/prod",
            "0195f48bd00070008000000000000099/x/prod",
            "not-a-uuid/x/prod",
            too_long.as_str(),
        ] {
            assert!(Scope::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_text_and_design_env_round_trip() {
        for env in Env::ALL {
            assert_eq!(Env::parse(env.as_str()), Some(env));
            let d: crate::design::Env = env.into();
            assert_eq!(Env::from(d), env);
        }
        assert_eq!(Env::from(crate::design::Env::Production), Env::Prod);
        assert_eq!(Env::parse("production"), None);
    }
}
