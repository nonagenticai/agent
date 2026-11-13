//! B8 T10 (plan § Revision 2026-10-25 P5): the two platform hooks B8 implements, wired into `AppState` in place of
//! `NoHooks`.
//!
//! - [`B8Hooks`] as [`GeneratorGuard`]: `tag_family` asks it for the red items of a family. An OPEN platform run whose
//!   output names the family reports its red findings (the run's own hydration, and from the Validate step on the
//!   department's grid findings, [`super::findings`]); a family no open run is working on reports nothing, so a
//!   plain `tagLayerRevisions` of seeded or hand-edited layers is not held back by journey rules the owner is not
//!   editing through the generator (Review Focus 3: "while a run on that layer is red").
//! - [`B8Hooks`] as [`ObjectContextSources`]: the journey activities an object appears in, read from the grid of
//!   every started industry department (an activity that binds the object, or expects an operation on it).
//!   `systems` is B7's list, built by the route; B8 adds none.
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::findings;
use super::store;
use crate::error::ApiResult;
use crate::journey::grid;
use crate::journey::model::{
    ActivityObject, Actor, JourneyActivity, ObjectRole, Severity, object_key,
};
use crate::platform::hooks::{BoxFuture, GeneratorGuard, ObjectContextSources};

/// The B8 implementation of the platform hooks.
#[derive(Debug, Clone, Copy, Default)]
pub struct B8Hooks;

impl GeneratorGuard for B8Hooks {
    fn red_items<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        family: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<String>>> {
        Box::pin(red_items(conn, family))
    }
}

impl ObjectContextSources for B8Hooks {
    fn systems(&self, _object: &str) -> Vec<Value> {
        Vec::new()
    }

    fn activities<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        object: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<Value>>> {
        Box::pin(activities(conn, object))
    }
}

/// The red findings of the open platform runs working on `family` (`run #12 (journey): stage purchase has no
/// activity`), run by run. Read on the caller's connection, so a run that turns red under the family lock is seen.
pub async fn red_items(conn: &mut PgConnection, family: &str) -> ApiResult<Vec<String>> {
    let ids = store::open_runs_on(conn, family).await?;
    let mut out = Vec::new();
    for id in ids {
        let d = store::detail(&mut *conn, id).await?;
        let all = findings::all(&mut *conn, &d, false).await?;
        for f in all.iter().filter(|f| f.severity == Severity::Red) {
            out.push(format!("run #{id} ({}): {}", d.run.current_step, f.message));
        }
    }
    Ok(out)
}

fn actor_str(a: Actor) -> &'static str {
    match a {
        Actor::Customer => "customer",
        Actor::Company => "company",
        Actor::Employee => "employee",
    }
}

fn binding<'a>(a: &'a JourneyActivity, object: &str) -> Option<&'a ActivityObject> {
    a.objects.iter().find(|o| object_key(&o.object) == object)
}

/// `ObjectActivityTouch`es of `object`: one per (industry, department, activity) that binds it or expects an
/// operation on it.
/// The operations of one activity on the object (`prefix` = `object.`), once each in first-use order: an activity
/// that uses two variants of one operation (`case.create{complaint}` and `{handover}`) lists it once, required when
/// any use is.
fn object_ops<'a>(ops: impl Iterator<Item = (&'a str, bool)>, prefix: &str) -> Vec<Value> {
    let mut out: Vec<(&str, bool)> = Vec::new();
    for (op, required) in ops.filter(|(op, _)| op.starts_with(prefix)) {
        match out.iter_mut().find(|(o, _)| *o == op) {
            Some(seen) => seen.1 |= required,
            None => out.push((op, required)),
        }
    }
    out.into_iter()
        .map(|(op, required)| json!({"op": op, "required": required}))
        .collect()
}

pub async fn activities(conn: &mut PgConnection, object: &str) -> ApiResult<Vec<Value>> {
    let depts: Vec<(String, String)> = sqlx::query_as(
        "SELECT industry, department FROM layer \
          WHERE scope = 'industry_dept' AND region IS NULL AND industry IS NOT NULL AND department IS NOT NULL \
          ORDER BY key",
    )
    .fetch_all(&mut *conn)
    .await?;
    let prefix = format!("{object}.");
    let mut out = Vec::new();
    for (industry, department) in depts {
        let j = match grid::load(&mut *conn, &industry, &department).await {
            Ok(j) => j,
            // a layer whose industry is gone: nothing to show
            Err(e) if e.code() == "journey_not_found" => continue,
            Err(e) => return Err(e),
        };
        for a in &j.activities {
            let bound = binding(a, object);
            let ops = object_ops(
                a.operations.iter().map(|o| (o.op.as_str(), o.required)),
                &prefix,
            );
            if bound.is_none() && ops.is_empty() {
                continue;
            }
            let variant = bound.and_then(|b| {
                let raw = b.object.as_str();
                raw.strip_prefix(object)
                    .and_then(|r| r.strip_prefix('{'))
                    .and_then(|r| r.strip_suffix('}'))
                    .map(str::to_owned)
            });
            out.push(json!({
                "industry": industry,
                "industry_label": j.label,
                "department": department,
                "stage": a.stage.as_str(),
                "key": a.key,
                "label": a.label,
                "actor": actor_str(a.actor),
                "role": match bound.map(|b| b.role) {
                    Some(ObjectRole::Context) => "context",
                    _ => "subject",
                },
                "variant": variant,
                "fields": bound.map(|b| b.fields.clone()).unwrap_or_default(),
                "operations": ops,
            }));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_activity_lists_each_object_operation_once() {
        let ops = [
            ("case.create", false),
            ("order.read", true),
            ("case.create", true),
            ("case.comment", false),
        ];
        assert_eq!(
            object_ops(ops.into_iter(), "case."),
            vec![
                json!({"op": "case.create", "required": true}),
                json!({"op": "case.comment", "required": false}),
            ]
        );
    }
}
