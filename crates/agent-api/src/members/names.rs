//! A person's name, one resolution for every read model: `users.display_name`, else `users.email`, else the sub
//! (blank counts as absent). B2 live acceptance (2026-10-21): the release, draft, rule and readiness views read
//! `display_name` only, so a member without one (an invited or signed-up person) showed as a raw Keycloak sub.
//! Reads run under the caller's tenant scope: a sub that is not a member of the tenant is named by the name it last
//! presented as staff (0115 `actor_names`, [`actor_seen`]: a platform super_admin is `<preferred_username>
//! (platform)`), else by itself. Queries that join `users` instead write the same order in SQL:
//! `coalesce(nullif(btrim(u.display_name), ''), nullif(btrim(u.email), ''), agent_actor_name(<sub>), <sub>)`.
use std::collections::BTreeMap;

use sqlx::PgConnection;

use crate::auth::Caller;
use crate::error::ApiResult;

/// Remember the name of a staff caller who has no `users` row in the tenant (B13 live acceptance: a super_admin acting
/// in a tenant showed as its Keycloak sub as handover assignee, in History and as transcript author). Called by every
/// staff write path that records the caller's sub, before it reads the caller's name. Only super_admins are recorded
/// (a member's `users` row names it, and wins); a token without `preferred_username` (actor = sub) records nothing.
pub async fn actor_seen(c: &mut PgConnection, caller: &Caller) -> ApiResult<()> {
    let actor = caller.actor.trim();
    if !caller.super_admin || actor.is_empty() || actor == caller.sub {
        return Ok(());
    }
    let name: String = format!("{actor} (platform)").chars().take(200).collect();
    sqlx::query("SELECT agent_actor_name_seen($1, $2)")
        .bind(&caller.sub)
        .bind(&name)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// The remembered name of a sub with no `users` row ([`actor_seen`]), else the sub.
async fn actor_name(c: &mut PgConnection, sub: &str) -> ApiResult<String> {
    let n: Option<String> = sqlx::query_scalar("SELECT agent_actor_name($1)")
        .bind(sub)
        .fetch_one(&mut *c)
        .await?;
    Ok(n.unwrap_or_else(|| sub.to_string()))
}

/// The name of `sub`: `display_name`, else `email`, else the sub.
pub fn resolve(sub: &str, display_name: Option<&str>, email: Option<&str>) -> String {
    [display_name, email]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|n| !n.is_empty())
        .unwrap_or(sub)
        .to_string()
}

/// The names of `subs` (every sub gets an entry; one query).
pub async fn lookup(c: &mut PgConnection, subs: &[String]) -> ApiResult<BTreeMap<String, String>> {
    let rows: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT sub, display_name, email FROM users WHERE sub = ANY($1)")
            .bind(subs)
            .fetch_all(&mut *c)
            .await?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (sub, display_name, email) in rows {
        let n = resolve(&sub, display_name.as_deref(), email.as_deref());
        out.insert(sub, n);
    }
    let missing: Vec<String> = subs
        .iter()
        .filter(|s| !out.contains_key(*s))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let found: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT s, agent_actor_name(s) FROM unnest($1::text[]) AS s")
                .bind(&missing)
                .fetch_all(&mut *c)
                .await?;
        for (sub, name) in found {
            let n = name.unwrap_or_else(|| sub.clone());
            out.insert(sub, n);
        }
        for s in missing {
            out.entry(s.clone()).or_insert(s);
        }
    }
    Ok(out)
}

/// The name of one sub.
pub async fn one(c: &mut PgConnection, sub: &str) -> ApiResult<String> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT display_name, email FROM users WHERE sub = $1")
            .bind(sub)
            .fetch_optional(&mut *c)
            .await?;
    Ok(match row {
        Some((display_name, email)) => resolve(sub, display_name.as_deref(), email.as_deref()),
        None => actor_name(c, sub).await?,
    })
}

/// [`one`], cached per request.
#[derive(Default)]
pub struct Names {
    cache: BTreeMap<String, String>,
}

impl Names {
    pub async fn of(&mut self, c: &mut PgConnection, sub: &str) -> ApiResult<String> {
        if let Some(n) = self.cache.get(sub) {
            return Ok(n.clone());
        }
        let n = one(c, sub).await?;
        self.cache.insert(sub.to_string(), n.clone());
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn display_name_then_email_then_sub() {
        assert_eq!(
            resolve("s1", Some("Ann Lee"), Some("ann@x.example")),
            "Ann Lee"
        );
        assert_eq!(resolve("s1", None, Some("ann@x.example")), "ann@x.example");
        assert_eq!(
            resolve("s1", Some("  "), Some("ann@x.example")),
            "ann@x.example"
        );
        assert_eq!(resolve("s1", Some(""), Some(" ")), "s1");
        assert_eq!(resolve("s1", None, None), "s1");
    }
}
