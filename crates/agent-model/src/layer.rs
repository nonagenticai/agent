//! Layers and scopes (configurator spec section 4.1), without the tenant scope (D8: tenant changes are
//! refinements, not layers). A layer's key is its label (`industry_dept/retail/customer_service`), exactly as the
//! web `layerLabel` writes it.
//!
//! D116 base-department layers: `base_dept/<d>` (`<d>` an archetype key) holds what every department of archetype
//! `<d>` shares whatever its industry; it folds at the archetype position of each department extending `<d>`.
//!
//! D111 region sub-layers: a base, industry, base_dept or industry_dept layer may carry a region sub-layer (`<r>` a
//! jurisdiction code): `base/region/<r>`, `industry/<i>/region/<r>`, `base_dept/<d>/region/<r>`,
//! `industry_dept/<i>/<d>/region/<r>`. A sub-layer has its
//! parent's scope (so it holds the same kinds), belongs to its parent's tag family ([`LayerKey::family`]) and folds
//! right after its parent (`compile::fold_order`) when the selection names its region.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The scope of a layer, ordered from the lowest (base) to the highest (job).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Base,
    Profile,
    Industry,
    /// D116: the industry-independent department layer `base_dept/<department>`.
    BaseDept,
    IndustryDept,
    Job,
}

impl Scope {
    pub const ALL: [Scope; 6] = [
        Scope::Base,
        Scope::Profile,
        Scope::Industry,
        Scope::BaseDept,
        Scope::IndustryDept,
        Scope::Job,
    ];

    /// Position of the scope in the compile order (base = 0 .. job = 5).
    pub fn rank(self) -> u8 {
        match self {
            Scope::Base => 0,
            Scope::Profile => 1,
            Scope::Industry => 2,
            Scope::BaseDept => 3,
            Scope::IndustryDept => 4,
            Scope::Job => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Base => "base",
            Scope::Profile => "profile",
            Scope::Industry => "industry",
            Scope::BaseDept => "base_dept",
            Scope::IndustryDept => "industry_dept",
            Scope::Job => "job",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.as_str() == s)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayerError {
    #[error("scope {scope:?} requires key `{key}`")]
    MissingKey { scope: Scope, key: &'static str },
    #[error("scope {scope:?} does not allow key `{key}`")]
    ExtraKey { scope: Scope, key: &'static str },
    #[error("key `{key}` must not be empty")]
    EmptyValue { key: &'static str },
    #[error("key `{key}` must not contain `/` (nor `=` in a dimension)")]
    BadValue { key: &'static str },
    #[error("unknown layer scope `{0}`")]
    UnknownScope(String),
    #[error("layer label `{0}` does not fit its scope")]
    BadLabel(String),
}

/// The layer key every authored row carries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayerKey {
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub industry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub department: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// `(key, value)` of a `key=value` dimension, e.g. `("channel", "email")`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<(String, String)>,
    /// D111: the region of a region sub-layer (`nl` in `industry/retail/region/nl`); only base, industry,
    /// base_dept (D116) and industry_dept layers may carry one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// The literal key part that introduces a region in a layer label (`…/region/<r>`).
pub const REGION_PART: &str = "region";

fn some(s: &str) -> Option<String> {
    Some(s.to_owned())
}

impl LayerKey {
    /// A key of `scope` with every other part unset (valid only for `base`).
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            industry: None,
            department: None,
            job: None,
            rule: None,
            dimension: None,
            region: None,
        }
    }

    pub fn base() -> Self {
        Self::new(Scope::Base)
    }

    pub fn profile(rule: &str) -> Self {
        Self {
            rule: some(rule),
            ..Self::new(Scope::Profile)
        }
    }

    pub fn industry(industry: &str) -> Self {
        Self {
            industry: some(industry),
            ..Self::new(Scope::Industry)
        }
    }

    /// D116: `base_dept/<department>`, the industry-independent layer of archetype `department`.
    pub fn base_dept(department: &str) -> Self {
        Self {
            department: some(department),
            ..Self::new(Scope::BaseDept)
        }
    }

    pub fn industry_dept(industry: &str, department: &str) -> Self {
        Self {
            industry: some(industry),
            department: some(department),
            ..Self::new(Scope::IndustryDept)
        }
    }

    pub fn job(job: &str) -> Self {
        Self {
            job: some(job),
            ..Self::new(Scope::Job)
        }
    }

    /// `job/<job>/<key>=<value>`, e.g. `job/customer_care_agent/channel=email`.
    pub fn job_dimension(job: &str, key: &str, value: &str) -> Self {
        Self {
            dimension: Some((key.to_owned(), value.to_owned())),
            ..Self::job(job)
        }
    }

    /// D111: the region sub-layer `<self>/region/<region>` of this (base, industry, base_dept or industry_dept)
    /// layer.
    pub fn with_region(&self, region: &str) -> Self {
        Self {
            region: some(region),
            ..self.clone()
        }
    }

    /// D111: whether this is a region sub-layer.
    pub fn is_region(&self) -> bool {
        self.region.is_some()
    }

    /// D111: the parent of a region sub-layer (the key without its region); the key itself otherwise.
    pub fn parent(&self) -> Self {
        Self {
            region: None,
            ..self.clone()
        }
    }

    /// Enforce the required-keys table of spec section 4.1 exactly: required keys must be present, optional
    /// keys may be, any other key is an error. Values are non-empty and cannot hold the label separator `/`
    /// (a dimension key or value neither `=`), so `parse_label(label())` gives the key back.
    pub fn validate(&self) -> Result<(), LayerError> {
        let (required, optional): (&[&str], &[&str]) = match self.scope {
            Scope::Base => (&[], &["region"]),
            Scope::Profile => (&["rule"], &[]),
            Scope::Industry => (&["industry"], &["region"]),
            Scope::BaseDept => (&["department"], &["region"]),
            Scope::IndustryDept => (&["industry", "department"], &["region"]),
            Scope::Job => (&["job"], &["dimension"]),
        };
        let empty = |s: &Option<String>| s.as_deref().is_some_and(str::is_empty);
        let bad = |s: &Option<String>| s.as_deref().is_some_and(|v| v.contains('/'));
        let present: [(&'static str, bool, bool, bool); 6] = [
            (
                "industry",
                self.industry.is_some(),
                empty(&self.industry),
                bad(&self.industry),
            ),
            (
                "department",
                self.department.is_some(),
                empty(&self.department),
                bad(&self.department),
            ),
            ("job", self.job.is_some(), empty(&self.job), bad(&self.job)),
            (
                "rule",
                self.rule.is_some(),
                empty(&self.rule),
                bad(&self.rule),
            ),
            (
                "dimension",
                self.dimension.is_some(),
                self.dimension
                    .as_ref()
                    .is_some_and(|(k, v)| k.is_empty() || v.is_empty()),
                self.dimension.as_ref().is_some_and(|(k, v)| {
                    k.contains('/') || v.contains('/') || k.contains('=') || v.contains('=')
                }),
            ),
            (
                "region",
                self.region.is_some(),
                empty(&self.region),
                self.region
                    .as_deref()
                    .is_some_and(|v| v.contains('/') || v.contains('=')),
            ),
        ];
        for (key, is_present, is_empty, is_bad) in present {
            let is_required = required.contains(&key);
            if is_required && !is_present {
                return Err(LayerError::MissingKey {
                    scope: self.scope,
                    key,
                });
            }
            if is_present && !is_required && !optional.contains(&key) {
                return Err(LayerError::ExtraKey {
                    scope: self.scope,
                    key,
                });
            }
            if is_empty {
                return Err(LayerError::EmptyValue { key });
            }
            if is_bad {
                return Err(LayerError::BadValue { key });
            }
        }
        Ok(())
    }

    /// The layer's name, as the web `layerLabel` writes it: the scope, then the set parts in the order
    /// industry, department, `region/<r>` (D111), job, rule, `key=value` dimension, joined by `/`.
    pub fn label(&self) -> String {
        self.to_string()
    }

    /// The inverse of [`LayerKey::label`]; the parsed key is validated.
    pub fn parse_label(label: &str) -> Result<LayerKey, LayerError> {
        let mut parts = label.split('/');
        let scope_part = parts.next().unwrap_or_default();
        let scope = Scope::parse(scope_part)
            .ok_or_else(|| LayerError::UnknownScope(scope_part.to_owned()))?;
        let rest: Vec<&str> = parts.collect();
        let bad = || LayerError::BadLabel(label.to_owned());
        let key = match (scope, rest.as_slice()) {
            (Scope::Base, []) => LayerKey::base(),
            (Scope::Base, [REGION_PART, region]) => LayerKey::base().with_region(region),
            (Scope::Profile, [rule]) => LayerKey::profile(rule),
            (Scope::Industry, [industry]) => LayerKey::industry(industry),
            (Scope::Industry, [industry, REGION_PART, region]) => {
                LayerKey::industry(industry).with_region(region)
            }
            (Scope::BaseDept, [department]) => LayerKey::base_dept(department),
            (Scope::BaseDept, [department, REGION_PART, region]) => {
                LayerKey::base_dept(department).with_region(region)
            }
            (Scope::IndustryDept, [industry, department]) => {
                LayerKey::industry_dept(industry, department)
            }
            (Scope::IndustryDept, [industry, department, REGION_PART, region]) => {
                LayerKey::industry_dept(industry, department).with_region(region)
            }
            (Scope::Job, [job]) => LayerKey::job(job),
            (Scope::Job, [job, dimension]) => {
                let (k, v) = dimension.split_once('=').ok_or_else(bad)?;
                LayerKey::job_dimension(job, k, v)
            }
            _ => return Err(bad()),
        };
        key.validate()?;
        Ok(key)
    }

    /// A template head (B6-3): base, industry, base_dept, industry_dept, or job without a dimension, and not a region
    /// sub-layer (D111). A head is its own tag family; tagging it tags its sublayers too.
    pub fn is_template_head(&self) -> bool {
        self.scope != Scope::Profile && self.dimension.is_none() && self.region.is_none()
    }

    /// The template family that tags this layer (B6-3): a head is its own family, a job × dimension layer
    /// belongs to its job, a region sub-layer to its parent (D111: one tag pins both). A profile layer has no family of its own: the family that owns its axis rule tags it
    /// (`axis_rule.owner`, default `base`), which is data, so `None` here.
    pub fn family(&self) -> Option<LayerKey> {
        match self.scope {
            Scope::Profile => None,
            Scope::Job if self.dimension.is_some() => Some(LayerKey {
                dimension: None,
                ..self.clone()
            }),
            _ => Some(self.parent()),
        }
    }
}

impl fmt::Display for LayerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.scope.as_str())?;
        let parts = [&self.industry, &self.department, &self.job, &self.rule];
        for (n, part) in parts.into_iter().enumerate() {
            if let Some(part) = part.as_deref().filter(|p| !p.is_empty()) {
                write!(f, "/{part}")?;
            }
            // D111: the region follows the industry / department parts
            if n == 1
                && let Some(region) = self.region.as_deref().filter(|r| !r.is_empty())
            {
                write!(f, "/{REGION_PART}/{region}")?;
            }
        }
        if let Some((k, v)) = &self.dimension {
            write!(f, "/{k}={v}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_industry_dept_requires_both() {
        let mut k = LayerKey::new(Scope::IndustryDept);
        assert!(k.validate().is_err());
        k.industry = Some("retail".into());
        assert_eq!(
            k.validate(),
            Err(LayerError::MissingKey {
                scope: Scope::IndustryDept,
                key: "department"
            })
        );
        k.department = Some("faq".into());
        assert_eq!(k.validate(), Ok(()));
    }

    #[test]
    fn layer_extra_key_rejected() {
        let mut k = LayerKey::base();
        assert_eq!(k.validate(), Ok(()));
        k.industry = Some("retail".into());
        assert_eq!(
            k.validate(),
            Err(LayerError::ExtraKey {
                scope: Scope::Base,
                key: "industry"
            })
        );
    }

    #[test]
    fn scope_rank_is_compile_order() {
        for (i, s) in Scope::ALL.iter().enumerate() {
            assert_eq!(usize::from(s.rank()), i);
            assert_eq!(Scope::parse(s.as_str()), Some(*s));
        }
        assert!(Scope::Base < Scope::Job);
        assert_eq!(Scope::parse("tenant"), None);
    }

    #[test]
    fn job_allows_optional_dimension() {
        let mut k = LayerKey::job_dimension("customer_care_agent", "channel", "voice");
        assert_eq!(k.validate(), Ok(()));
        k.rule = Some("r".into());
        assert!(k.validate().is_err());
    }

    #[test]
    fn profile_scope_needs_rule() {
        assert_eq!(
            LayerKey::new(Scope::Profile).validate(),
            Err(LayerError::MissingKey {
                scope: Scope::Profile,
                key: "rule"
            })
        );
        assert_eq!(LayerKey::profile("bundles").validate(), Ok(()));
        assert_eq!(
            LayerKey::profile("").validate(),
            Err(LayerError::EmptyValue { key: "rule" })
        );
        let mut with_industry = LayerKey::profile("bundles");
        with_industry.industry = Some("retail".into());
        assert!(with_industry.validate().is_err());
        assert!(LayerKey::parse_label("profile").is_err());
        assert_eq!(
            LayerKey::parse_label("profile/bundles"),
            Ok(LayerKey::profile("bundles"))
        );
        // A profile layer is tagged by the family owning its rule (data), not by itself.
        assert_eq!(LayerKey::profile("bundles").family(), None);
        assert!(!LayerKey::profile("bundles").is_template_head());
    }

    #[test]
    fn job_dimension_family_is_job() {
        let sub = LayerKey::job_dimension("customer_care_agent", "channel", "email");
        assert_eq!(sub.family(), Some(LayerKey::job("customer_care_agent")));
        assert!(!sub.is_template_head());
        let head = LayerKey::job("customer_care_agent");
        assert_eq!(head.family(), Some(head.clone()));
        assert!(head.is_template_head());
        let dept = LayerKey::industry_dept("retail", "customer_service");
        assert_eq!(dept.family(), Some(dept.clone()));
        assert_eq!(LayerKey::base().family(), Some(LayerKey::base()));
    }

    /// The 16 layers of `web/src/lib/fixtures/platform.ts`, labelled as the web `layerLabel` labels them.
    #[test]
    fn layer_label_round_trips() {
        let fixture = [
            (LayerKey::base(), "base"),
            (
                LayerKey::profile("metered_supply"),
                "profile/metered_supply",
            ),
            (
                LayerKey::profile("implicit_renewal"),
                "profile/implicit_renewal",
            ),
            (
                LayerKey::profile("access_products"),
                "profile/access_products",
            ),
            (
                LayerKey::profile("service_appointments"),
                "profile/service_appointments",
            ),
            (LayerKey::profile("bundles"), "profile/bundles"),
            (LayerKey::industry("retail"), "industry/retail"),
            (LayerKey::industry("utilities"), "industry/utilities"),
            (LayerKey::industry("it_services"), "industry/it_services"),
            (
                LayerKey::industry_dept("retail", "customer_service"),
                "industry_dept/retail/customer_service",
            ),
            (
                LayerKey::industry_dept("utilities", "billing"),
                "industry_dept/utilities/billing",
            ),
            (
                LayerKey::industry_dept("it_services", "it_service_desk"),
                "industry_dept/it_services/it_service_desk",
            ),
            (
                LayerKey::job("customer_care_agent"),
                "job/customer_care_agent",
            ),
            (
                LayerKey::job_dimension("customer_care_agent", "channel", "email"),
                "job/customer_care_agent/channel=email",
            ),
            (
                LayerKey::job("it_service_desk_agent"),
                "job/it_service_desk_agent",
            ),
            (LayerKey::job("billing_assistant"), "job/billing_assistant"),
        ];
        assert_eq!(fixture.len(), 16);
        for (key, label) in &fixture {
            assert_eq!(key.validate(), Ok(()), "{label}");
            assert_eq!(key.label(), *label);
            assert_eq!(LayerKey::parse_label(label).as_ref(), Ok(key), "{label}");
            assert_eq!(
                LayerKey::parse_label(&key.label()).map(|k| k.label()),
                Ok((*label).to_owned())
            );
        }
        let json = serde_json::to_value(&fixture[13].0).expect("serialises");
        assert_eq!(
            json,
            serde_json::json!({"scope": "job", "job": "customer_care_agent", "dimension": ["channel", "email"]})
        );
    }

    /// D111: the region sub-layer grammar, its family and its validation.
    #[test]
    fn region_sublayers_round_trip_and_join_their_parent_family() {
        for (key, label, parent) in [
            (LayerKey::base().with_region("nl"), "base/region/nl", "base"),
            (
                LayerKey::industry("retail").with_region("nl"),
                "industry/retail/region/nl",
                "industry/retail",
            ),
            (
                LayerKey::industry_dept("retail", "customer_service").with_region("de"),
                "industry_dept/retail/customer_service/region/de",
                "industry_dept/retail/customer_service",
            ),
        ] {
            assert_eq!(key.validate(), Ok(()), "{label}");
            assert_eq!(key.label(), label);
            assert_eq!(LayerKey::parse_label(label).as_ref(), Ok(&key), "{label}");
            assert!(key.is_region());
            assert!(!key.is_template_head());
            assert_eq!(key.family().map(|f| f.label()).as_deref(), Some(parent));
            assert_eq!(key.parent().label(), parent);
        }
        // an industry named `region` stays an industry
        assert_eq!(
            LayerKey::parse_label("industry/region"),
            Ok(LayerKey::industry("region"))
        );
        // only base / industry / industry_dept carry a region
        let mut job = LayerKey::job("care");
        job.region = Some("nl".into());
        assert_eq!(
            job.validate(),
            Err(LayerError::ExtraKey {
                scope: Scope::Job,
                key: "region"
            })
        );
        let mut profile = LayerKey::profile("bundles");
        profile.region = Some("nl".into());
        assert!(profile.validate().is_err());
        assert_eq!(
            LayerKey::base().with_region("").validate(),
            Err(LayerError::EmptyValue { key: "region" })
        );
        assert_eq!(
            LayerKey::base().with_region("n=l").validate(),
            Err(LayerError::BadValue { key: "region" })
        );
        for bad in [
            "base/region",
            "base/region/",
            "base/area/nl",
            "industry/retail/region",
            "industry/retail/regio/nl",
            "industry_dept/retail/region/nl",
            "industry_dept/retail/cs/region",
            "industry_dept/retail/cs/region/nl/x",
            "job/care/region/nl",
            "profile/bundles/region/nl",
        ] {
            assert!(LayerKey::parse_label(bad).is_err(), "{bad:?}");
        }
        let json = serde_json::to_value(LayerKey::industry("retail").with_region("nl")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"scope": "industry", "industry": "retail", "region": "nl"})
        );
    }

    /// D116: the base_dept grammar, its family, its region sub-layer and its rank.
    #[test]
    fn base_dept_layers_round_trip() {
        let head = LayerKey::base_dept("customer_service");
        assert_eq!(head.validate(), Ok(()));
        assert_eq!(head.label(), "base_dept/customer_service");
        assert_eq!(
            LayerKey::parse_label("base_dept/customer_service"),
            Ok(head.clone())
        );
        assert!(head.is_template_head());
        assert_eq!(head.family(), Some(head.clone()));
        assert_eq!(head.industry, None);
        assert_eq!(head.job, None);
        let sub = head.with_region("nl");
        assert_eq!(sub.label(), "base_dept/customer_service/region/nl");
        assert_eq!(
            LayerKey::parse_label("base_dept/customer_service/region/nl"),
            Ok(sub.clone())
        );
        assert!(!sub.is_template_head());
        assert_eq!(sub.parent(), head);
        assert_eq!(sub.family(), Some(head.clone()));
        let mut with_industry = LayerKey::base_dept("customer_service");
        with_industry.industry = Some("retail".into());
        assert_eq!(
            with_industry.validate(),
            Err(LayerError::ExtraKey {
                scope: Scope::BaseDept,
                key: "industry"
            })
        );
        assert_eq!(
            LayerKey::new(Scope::BaseDept).validate(),
            Err(LayerError::MissingKey {
                scope: Scope::BaseDept,
                key: "department"
            })
        );
        for bad in [
            "base_dept",
            "base_dept/a/b",
            "base_dept/a/region",
            "base_dept/a/region/nl/x",
        ] {
            assert!(LayerKey::parse_label(bad).is_err(), "{bad:?}");
        }
        assert!(Scope::Industry < Scope::BaseDept && Scope::BaseDept < Scope::IndustryDept);
        assert_eq!(Scope::BaseDept.rank(), 3);
        assert_eq!(Scope::parse("base_dept"), Some(Scope::BaseDept));
    }

    #[test]
    fn parse_label_rejects_malformed() {
        for bad in [
            "",
            "tenant/x",
            "base/x",
            "industry",
            "industry/",
            "industry_dept/retail",
            "industry_dept/retail/cs/x",
            "job/a/channel",
            "job/a/=email",
            "job/a/b=c/d",
            "profile/a/b",
        ] {
            assert!(LayerKey::parse_label(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            LayerKey::parse_label("tenant/x"),
            Err(LayerError::UnknownScope("tenant".into()))
        );
        assert_eq!(
            LayerKey::industry("a/b").validate(),
            Err(LayerError::BadValue { key: "industry" })
        );
        assert_eq!(
            LayerKey::job_dimension("j", "a=b", "c").validate(),
            Err(LayerError::BadValue { key: "dimension" })
        );
    }
}
