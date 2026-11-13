//! Authored writes (plan B6 Revision 2026-10-22 R5/R6): the checks a person's edit needs on top of the store.
//!
//! - [`write_authored`]: [`super::rows::write_revision`] behind the optimistic checks of `writeLayerRow` and every
//!   B6b `rev` route: 409 `duplicate_add` / `add_tombstoned` for an add, 409 `stale_revision` / 404 `row_not_found`
//!   for a patch or tombstone. `write_revision` itself is unchanged (KH rewrites rows.rs; its callers stay as they
//!   are).
//! - [`insert_layer`]: one new `layer` row (`createLayer`, the profile layer of `createAxisRule`), checked before
//!   the INSERT so the client gets the plan's codes and not a constraint 500.
//!
//! Both run in the caller's `agent_platform` transaction and take the family lock first.
use agent_model::kinds::{Kind, Op};
use agent_model::layer::{LayerKey, Scope};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgConnection;

use super::layers::{self, LAYER_COLUMNS, Layer};
use super::lock_family;
use super::rows::{self, Row};
use crate::audit::Actor;
use crate::error::{ApiError, ApiResult};

/// What the author expects to overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// An `add`: the key has no current revision in this layer.
    New,
    /// A `patch` or `tombstone`: the current revision is this `rev` (the client's `expected_rev`).
    Rev(i32),
}

impl Expect {
    /// The expectation of a request body's `op` + `expected_rev`: `add` takes none, `patch` / `tombstone` need one.
    /// 422 `invalid_row` otherwise.
    pub fn for_op(op: Op, expected_rev: Option<i32>) -> ApiResult<Expect> {
        match (op, expected_rev) {
            (Op::Add, None) => Ok(Expect::New),
            (Op::Add, Some(_)) => Err(ApiError::Unprocessable(
                "invalid_row",
                "An add takes no expected_rev.".into(),
            )),
            (_, Some(n)) if n >= 1 => Ok(Expect::Rev(n)),
            (_, Some(_)) => Err(ApiError::Unprocessable(
                "invalid_row",
                "expected_rev must be 1 or more.".into(),
            )),
            (_, None) => Err(ApiError::Unprocessable(
                "invalid_row",
                format!("A {} needs expected_rev.", op.as_str()),
            )),
        }
    }
}

/// The current revision of a (layer, kind, key): its `rev` and whether it is a tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Current {
    pub rev: i32,
    pub tombstone: bool,
}

/// The pure check behind [`write_authored`]. A tombstoned key reads as absent for a patch or tombstone (it is
/// retired: 404 `row_not_found`) and blocks an add (409 `add_tombstoned`: retired keys are not reused).
pub fn check_expect(
    current: Option<Current>,
    expect: Expect,
    kind: Kind,
    key: &str,
) -> ApiResult<()> {
    match (expect, current) {
        (Expect::New, None) => Ok(()),
        (Expect::New, Some(c)) if c.tombstone => Err(ApiError::Conflict(
            "add_tombstoned",
            format!("The {kind} {key} was retired here; its key cannot be added again."),
        )
        .with("current_rev", c.rev)),
        (Expect::New, Some(c)) => Err(ApiError::Conflict(
            "duplicate_add",
            format!("The {kind} {key} already exists here; edit it instead."),
        )
        .with("current_rev", c.rev)),
        (Expect::Rev(_), None) => Err(row_not_found(kind, key)),
        (Expect::Rev(_), Some(c)) if c.tombstone => Err(row_not_found(kind, key)),
        (Expect::Rev(n), Some(c)) if c.rev != n => Err(ApiError::Conflict(
            "stale_revision",
            format!(
                "The {kind} {key} changed since you opened it (now revision {}, you edited {n}). Reload and retry.",
                c.rev
            ),
        )
        .with("current_rev", c.rev)),
        (Expect::Rev(_), Some(_)) => Ok(()),
    }
}

fn row_not_found(kind: Kind, key: &str) -> ApiError {
    ApiError::Missing("row_not_found", format!("No current {kind} {key} here."))
}

/// The current revision of (`layer`, `kind`, `key`), if any.
pub async fn current_revision(
    conn: &mut PgConnection,
    layer: &Layer,
    kind: Kind,
    key: &str,
) -> ApiResult<Option<Current>> {
    let row: Option<(i32, String)> = sqlx::query_as(
        "SELECT rev, op FROM layer_row WHERE layer_id = $1 AND kind = $2 AND key = $3 AND is_current",
    )
    .bind(layer.id)
    .bind(kind.as_str())
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(rev, op)| Current {
        rev,
        tombstone: op == Op::Tombstone.as_str(),
    }))
}

/// [`rows::write_revision`] after the family lock and [`check_expect`] against the current revision. 404
/// `layer_not_found`; 409 `duplicate_add` / `add_tombstoned` / `stale_revision` (with `current_rev`); 404
/// `row_not_found`; then `write_revision`'s own 422s. The lock is re-entrant, so `write_revision` taking it again
/// is a no-op wait.
#[allow(clippy::too_many_arguments)]
pub async fn write_authored(
    conn: &mut PgConnection,
    actor: &Actor,
    layer_key: &str,
    kind: Kind,
    key: &str,
    op: Op,
    fields: Value,
    author: &str,
    expect: Expect,
) -> ApiResult<Row> {
    let layer = layers::layer_by_key(&mut *conn, layer_key).await?;
    lock_family(&mut *conn, &layer.family).await?;
    let current = current_revision(&mut *conn, &layer, kind, key).await?;
    check_expect(current, expect, kind, key)?;
    rows::write_revision(conn, actor, layer_key, kind, key, op, fields, author).await
}

/// The metadata of a new layer (B6-9; not versioned). The schema's scope CHECKs apply: `wave_label` only on an
/// industry, `extends` / `areas` only on an industry_dept, `template_selection` only on a job or industry_dept.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerMeta {
    pub label: Option<String>,
    pub rollout_wave: Option<i16>,
    pub wave_label: Option<String>,
    pub extends: Option<String>,
    #[serde(default)]
    pub areas: Vec<String>,
    pub template_selection: Option<Value>,
}

/// The longest layer label (0041 `layer.label`).
pub const MAX_LAYER_LABEL: usize = 120;

/// 422 `invalid_label` unless the label is 1..=120 characters after trimming.
pub fn check_label(label: &str) -> ApiResult<()> {
    let n = label.trim().chars().count();
    if (1..=MAX_LAYER_LABEL).contains(&n) {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(
            "invalid_label",
            format!("A label is 1 to {MAX_LAYER_LABEL} characters."),
        ))
    }
}

/// 422 `invalid_layer_key` for metadata the scope does not take (the 0041 scope CHECKs, as a client error).
pub fn check_meta(key: &LayerKey, meta: &LayerMeta) -> ApiResult<()> {
    let bad = |what: &str| {
        Err(ApiError::Unprocessable(
            "invalid_layer_key",
            format!("A {} layer takes no {what}.", key.scope),
        ))
    };
    if let Some(l) = &meta.label {
        check_label(l)?;
    }
    // D111 (0139 `layer_region_metadata`): the template metadata stays on the parent of a region sub-layer
    if key.is_region()
        && (meta.rollout_wave.is_some()
            || meta.wave_label.is_some()
            || meta.extends.is_some()
            || !meta.areas.is_empty()
            || meta.template_selection.is_some())
    {
        return Err(ApiError::Unprocessable(
            "invalid_layer_key",
            "A region sub-layer takes no rollout_wave, wave_label, extends, areas or template_selection; they \
             stay on its parent."
                .into(),
        ));
    }
    if meta.rollout_wave.is_some_and(|w| !(1..=3).contains(&w)) {
        return Err(ApiError::Unprocessable(
            "invalid_layer_key",
            "rollout_wave is 1, 2 or 3.".into(),
        ));
    }
    if meta.wave_label.is_some() && key.scope != Scope::Industry {
        return bad("wave_label");
    }
    if meta.extends.is_some() && key.scope != Scope::IndustryDept {
        return bad("extends");
    }
    if !meta.areas.is_empty() && key.scope != Scope::IndustryDept {
        return bad("areas");
    }
    if meta.template_selection.is_some() && !matches!(key.scope, Scope::Job | Scope::IndustryDept) {
        return bad("template_selection");
    }
    if meta
        .template_selection
        .as_ref()
        .is_some_and(|v| !v.is_object())
    {
        return Err(ApiError::Unprocessable(
            "invalid_layer_key",
            "template_selection is an object.".into(),
        ));
    }
    Ok(())
}

/// The family that tags a new layer (B6-3): a head is its own; a job × dimension layer belongs to its job; a
/// profile layer to `owner` (the axis rule's tagging family, D99), default `base`.
pub fn family_of(key: &LayerKey, owner: Option<&str>) -> String {
    match key.family() {
        Some(f) => f.label(),
        None => owner.unwrap_or("base").to_string(),
    }
}

/// The layer that must exist before `key` can be created: the parent of a region sub-layer (D111), `base` for an
/// industry or a base_dept (D116), `industry/<i>` for an industry_dept, `job/<j>` for a job × dimension layer, the
/// owning family for a profile; none for base or a job head.
pub fn parent_of(key: &LayerKey, owner: Option<&str>) -> Option<String> {
    if key.is_region() {
        return Some(key.parent().label());
    }
    match key.scope {
        Scope::Base => None,
        Scope::Industry | Scope::BaseDept => Some(LayerKey::base().label()),
        Scope::IndustryDept => key
            .industry
            .as_deref()
            .map(|i| LayerKey::industry(i).label()),
        Scope::Job if key.dimension.is_some() => key.family().map(|f| f.label()),
        Scope::Job => None,
        Scope::Profile => Some(family_of(key, owner)),
    }
}

/// Insert one layer: 422 `invalid_layer_key` (key or metadata; a region sub-layer's region not in the platform's
/// region vocabulary, D111), `invalid_label`; 404 `parent_layer_not_found` (the parent layer, with `parent`; or an
/// industry_dept's `extends` archetype not current in base, with `archetype`, D107); 409 `layer_exists`. Takes the
/// family lock. No audit row: the route records `layer.create`.
pub async fn insert_layer(
    conn: &mut PgConnection,
    key: &LayerKey,
    owner: Option<&str>,
    meta: &LayerMeta,
) -> ApiResult<Layer> {
    key.validate().map_err(|e| {
        ApiError::Unprocessable("invalid_layer_key", format!("Not a layer key: {e}."))
    })?;
    check_meta(key, meta)?;
    if let Some(region) = key.region.as_deref() {
        let known = crate::platform::templates::region_vocabulary(&mut *conn).await?;
        if !known.iter().any(|k| k == region) {
            return Err(ApiError::Unprocessable(
                "invalid_layer_key",
                format!("“{region}” is not a platform region (the region dimension's values)."),
            )
            .with("regions", known));
        }
    }
    let label = key.label();
    let family = family_of(key, owner);
    if let Some(parent) = parent_of(key, owner)
        && layers::find_layer_by_key(&mut *conn, &parent)
            .await?
            .is_none()
    {
        return Err(ApiError::Missing(
            "parent_layer_not_found",
            format!("Create {parent} before {label}."),
        )
        .with("parent", parent));
    }
    if key.scope == Scope::IndustryDept
        && let Some(archetype) = meta.extends.as_deref().filter(|a| !a.is_empty())
    {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM layer_row r JOIN layer l ON l.id = r.layer_id \
                             WHERE l.key = 'base' AND r.kind = 'department_archetype' AND r.key = $1 \
                               AND r.is_current AND r.op <> 'tombstone')",
        )
        .bind(archetype)
        .fetch_one(&mut *conn)
        .await?;
        if !exists {
            return Err(ApiError::Missing(
                "parent_layer_not_found",
                format!("{label} extends the archetype {archetype}, which base does not define."),
            )
            .with("archetype", archetype));
        }
    }
    // A head's family row is the one being inserted; lock on its own key either way.
    lock_family(&mut *conn, &family).await?;
    if layers::find_layer_by_key(&mut *conn, &label)
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(
            "layer_exists",
            format!("The layer {label} already exists."),
        ));
    }
    let (dimension_key, dimension_value) = match &key.dimension {
        Some((k, v)) => (Some(k.as_str()), Some(v.as_str())),
        None => (None, None),
    };
    Ok(sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO layer (key, scope, industry, department, job, rule, dimension_key, dimension_value, family, \
                            label, rollout_wave, wave_label, extends, areas, template_selection, region) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
         RETURNING {LAYER_COLUMNS}"
    )))
    .bind(&label)
    .bind(key.scope.as_str())
    .bind(key.industry.as_deref())
    .bind(key.department.as_deref())
    .bind(key.job.as_deref())
    .bind(key.rule.as_deref())
    .bind(dimension_key)
    .bind(dimension_value)
    .bind(&family)
    .bind(meta.label.as_deref().map(str::trim))
    .bind(meta.rollout_wave)
    .bind(meta.wave_label.as_deref())
    .bind(meta.extends.as_deref())
    .bind(&meta.areas)
    .bind(meta.template_selection.clone())
    .bind(key.region.as_deref())
    .fetch_one(&mut *conn)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: Kind = Kind::Object;

    fn cur(rev: i32, tombstone: bool) -> Option<Current> {
        Some(Current { rev, tombstone })
    }

    fn code(r: ApiResult<()>) -> &'static str {
        r.err().map(|e| e.code()).unwrap_or("ok")
    }

    #[test]
    fn add_checks() {
        assert_eq!(code(check_expect(None, Expect::New, K, "order")), "ok");
        assert_eq!(
            code(check_expect(cur(2, false), Expect::New, K, "order")),
            "duplicate_add"
        );
        assert_eq!(
            code(check_expect(cur(3, true), Expect::New, K, "order")),
            "add_tombstoned"
        );
    }

    #[test]
    fn rev_checks() {
        assert_eq!(
            code(check_expect(cur(2, false), Expect::Rev(2), K, "o")),
            "ok"
        );
        assert_eq!(
            code(check_expect(cur(3, false), Expect::Rev(2), K, "o")),
            "stale_revision"
        );
        assert_eq!(
            code(check_expect(None, Expect::Rev(1), K, "o")),
            "row_not_found"
        );
        assert_eq!(
            code(check_expect(cur(4, true), Expect::Rev(4), K, "o")),
            "row_not_found"
        );
    }

    #[test]
    fn stale_carries_current_rev() {
        let e = check_expect(cur(5, false), Expect::Rev(2), K, "o").unwrap_err();
        let ApiError::Detailed { extra, .. } = e else {
            panic!("not detailed")
        };
        assert_eq!(extra.get("current_rev"), Some(&serde_json::json!(5)));
    }

    #[test]
    fn expect_for_op() {
        assert_eq!(Expect::for_op(Op::Add, None).unwrap(), Expect::New);
        assert_eq!(Expect::for_op(Op::Patch, Some(3)).unwrap(), Expect::Rev(3));
        assert_eq!(
            Expect::for_op(Op::Tombstone, Some(1)).unwrap(),
            Expect::Rev(1)
        );
        for (op, rev) in [
            (Op::Add, Some(1)),
            (Op::Patch, None),
            (Op::Tombstone, None),
            (Op::Patch, Some(0)),
        ] {
            assert_eq!(
                Expect::for_op(op, rev).unwrap_err().code(),
                "invalid_row",
                "{op:?} {rev:?}"
            );
        }
    }

    #[test]
    fn families_and_parents() {
        let dept = LayerKey::industry_dept("retail", "customer_service");
        assert_eq!(
            family_of(&dept, None),
            "industry_dept/retail/customer_service"
        );
        assert_eq!(parent_of(&dept, None).as_deref(), Some("industry/retail"));
        let dim = LayerKey::job_dimension("care", "channel", "email");
        assert_eq!(family_of(&dim, None), "job/care");
        assert_eq!(parent_of(&dim, None).as_deref(), Some("job/care"));
        let prof = LayerKey::profile("bundles");
        assert_eq!(family_of(&prof, None), "base");
        assert_eq!(family_of(&prof, Some("industry/retail")), "industry/retail");
        assert_eq!(
            parent_of(&prof, Some("industry/retail")).as_deref(),
            Some("industry/retail")
        );
        assert_eq!(
            parent_of(&LayerKey::industry("retail"), None).as_deref(),
            Some("base")
        );
        assert_eq!(parent_of(&LayerKey::job("care"), None), None);
        assert_eq!(parent_of(&LayerKey::base(), None), None);
        assert_eq!(
            parent_of(&LayerKey::base_dept("cs"), None).as_deref(),
            Some("base")
        );
        // D111: a region sub-layer needs its parent and joins its family
        let nl = LayerKey::industry("retail").with_region("nl");
        assert_eq!(family_of(&nl, None), "industry/retail");
        assert_eq!(parent_of(&nl, None).as_deref(), Some("industry/retail"));
        let base_nl = LayerKey::base().with_region("nl");
        assert_eq!(family_of(&base_nl, None), "base");
        assert_eq!(parent_of(&base_nl, None).as_deref(), Some("base"));
        let dept_nl = dept.with_region("nl");
        assert_eq!(
            parent_of(&dept_nl, None).as_deref(),
            Some("industry_dept/retail/customer_service")
        );
    }

    #[test]
    fn base_dept_takes_no_extends_areas_or_selection() {
        let key = LayerKey::base_dept("cs");
        assert!(check_meta(&key, &LayerMeta::default()).is_ok());
        for meta in [
            LayerMeta {
                extends: Some("customer_service".into()),
                ..LayerMeta::default()
            },
            LayerMeta {
                areas: vec!["returns".into()],
                ..LayerMeta::default()
            },
            LayerMeta {
                template_selection: Some(serde_json::json!({})),
                ..LayerMeta::default()
            },
        ] {
            assert_eq!(
                check_meta(&key, &meta).unwrap_err().code(),
                "invalid_layer_key"
            );
        }
    }

    #[test]
    fn meta_checks() {
        let ind = LayerKey::industry("retail");
        let dept = LayerKey::industry_dept("retail", "cs");
        let ok = LayerMeta {
            label: Some(" Retail ".into()),
            wave_label: Some("Wave 1".into()),
            rollout_wave: Some(1),
            ..LayerMeta::default()
        };
        assert!(check_meta(&ind, &ok).is_ok());
        assert_eq!(
            check_meta(&dept, &ok).unwrap_err().code(),
            "invalid_layer_key"
        );
        let areas = LayerMeta {
            areas: vec!["returns".into()],
            ..LayerMeta::default()
        };
        assert!(check_meta(&dept, &areas).is_ok());
        assert_eq!(
            check_meta(&ind, &areas).unwrap_err().code(),
            "invalid_layer_key"
        );
        let blank = LayerMeta {
            label: Some("  ".into()),
            ..LayerMeta::default()
        };
        assert_eq!(
            check_meta(&ind, &blank).unwrap_err().code(),
            "invalid_label"
        );
        let wave = LayerMeta {
            rollout_wave: Some(4),
            ..LayerMeta::default()
        };
        assert_eq!(
            check_meta(&ind, &wave).unwrap_err().code(),
            "invalid_layer_key"
        );
        let sel = LayerMeta {
            template_selection: Some(serde_json::json!([])),
            ..LayerMeta::default()
        };
        assert_eq!(
            check_meta(&LayerKey::job("care"), &sel).unwrap_err().code(),
            "invalid_layer_key"
        );
        let region = LayerKey::industry_dept("retail", "cs").with_region("nl");
        assert_eq!(
            check_meta(&region, &areas).unwrap_err().code(),
            "invalid_layer_key"
        );
        let labelled = LayerMeta {
            label: Some("Retail NL".into()),
            ..LayerMeta::default()
        };
        assert!(check_meta(&region, &labelled).is_ok());
    }
}
