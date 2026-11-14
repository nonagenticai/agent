//! Keycloak admin API for the portal realm. Ported from strata_configurator `keycloak_admin.rs` (after
//! commonpractice `services/keycloak-admin.ts`), with the early-access v2 deltas (contract C5): the approve
//! path creates the identity itself WITHOUT an email, and the one email goes out last, after the user's
//! `locale` / `signup_app` attributes are written, via execute-actions-email with the app's `client_id` and
//! `redirect_uri`.
//!
//! The portal realm is shared with every other app, so the writes are deliberately narrow: create an identity
//! for an approved requester, merge two attributes, send the action email. It NEVER writes realm roles or a
//! `tenant_id` attribute: tenancy lives in agent's own `users` table. Every call is fatal on failure, the
//! lookup included: "could not ask" must never read as "no such user".
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::config::KeycloakAdminConfig;
use crate::http_base::BaseUrl;
use crate::service_token::{ServiceToken, UpstreamError};

/// The actions the set-password email asks for: set a password, confirm the address.
pub const INVITE_ACTIONS: [&str; 2] = ["UPDATE_PASSWORD", "VERIFY_EMAIL"];
/// The access email for an identity that can already sign in (commonpractice sends its own "sign in"
/// mail there; this service has no mailer, so Keycloak's action email carries it): confirm the address,
/// which is a no-op for a verified one, and land in the app.
pub const ACCESS_ACTIONS: [&str; 1] = ["VERIFY_EMAIL"];

/// Whether an identity can sign in yet (commonpractice `identityLoginState` / `setupNeeded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginState {
    /// No password credential, or `UPDATE_PASSWORD` pending: it cannot sign in until it sets one.
    pub needs_setup: bool,
    pub email_verified: bool,
}

#[derive(Deserialize)]
struct UserState {
    #[serde(rename = "emailVerified", default)]
    email_verified: bool,
    #[serde(rename = "requiredActions", default)]
    required_actions: Vec<String>,
}

#[derive(Deserialize)]
struct Credential {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RealmUser {
    id: String,
    username: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct FullUser {
    id: String,
    username: Option<String>,
    email: Option<String>,
    #[serde(rename = "firstName")]
    first_name: Option<String>,
    #[serde(rename = "lastName")]
    last_name: Option<String>,
}

#[derive(Deserialize)]
struct WireRole {
    name: String,
}

/// A portal identity as tenant admin needs it: its id (`sub`), email and display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalUser {
    pub sub: String,
    pub email: Option<String>,
    /// `firstName lastName`, else the username.
    pub name: Option<String>,
}

/// What [`KeycloakAdmin::execute_actions_email`] sends the person back to.
pub struct ActionsEmail<'a> {
    pub actions: &'a [&'a str],
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub lifespan_seconds: u64,
}

pub struct KeycloakAdmin {
    /// `.../admin/realms/portal`, parsed and checked once (http(s), no query or fragment), no trailing slash.
    base: BaseUrl,
    token: ServiceToken,
    http: reqwest::Client,
}

impl KeycloakAdmin {
    pub fn new(cfg: &KeycloakAdminConfig, http: reqwest::Client) -> Self {
        Self {
            base: cfg.api_url.clone(),
            token: ServiceToken::from_creds(&cfg.creds, http.clone()),
            http,
        }
    }

    /// `req` with the service account's bearer token, sent.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<reqwest::Response, UpstreamError> {
        let token = self.token.get().await?;
        let op = crate::observability::op_label("keycloak ", what);
        crate::observability::upstream_http("keycloak", op, req.bearer_auth(token).send())
            .await
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    /// The id of the realm user whose email, or else username, is exactly `email` (Keycloak creates users
    /// with `username == email`, and its 409 on create can come from either field). Keycloak's `exact=true`
    /// is still case-insensitive, so the match is re-checked here.
    pub async fn find_by_email(&self, email: &str) -> Result<Option<String>, UpstreamError> {
        for field in ["email", "username"] {
            let url = self.base.endpoint_with_query(
                &["users"],
                &[(field, email), ("exact", "true"), ("max", "2")],
            );
            let what = format!("keycloak user lookup by {field}");
            let resp = self.send(self.http.get(url), &what).await?;
            if !resp.status().is_success() {
                return Err(UpstreamError::status(&what, resp.status()));
            }
            let hits: Vec<RealmUser> = resp
                .json()
                .await
                .map_err(|e| UpstreamError::transport(&what, &e))?;
            let hit = hits.into_iter().find(|u| {
                let v = if field == "email" {
                    &u.email
                } else {
                    &u.username
                };
                v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(email))
            });
            if let Some(u) = hit {
                return Ok(Some(u.id));
            }
        }
        Ok(None)
    }

    /// Create an enabled realm user for `email` (no email is sent) and return its id (from the `Location`
    /// header); `None` when Keycloak answers 409 (the user exists).
    pub async fn create_user(
        &self,
        email: &str,
        first_name: Option<&str>,
        last_name: Option<&str>,
        attributes: &[(&str, &str)],
    ) -> Result<Option<String>, UpstreamError> {
        // No roles and no tenant attribute: the identity only authenticates (see the module doc). `attributes` are
        // born with it (a signup's ownership stamp, so a create whose answer was lost is recognised later).
        let mut rep =
            json!({"username": email, "email": email, "enabled": true, "emailVerified": false});
        if !attributes.is_empty() {
            let map: Map<String, Value> = attributes
                .iter()
                .map(|(k, v)| ((*k).to_string(), json!([v])))
                .collect();
            rep["attributes"] = Value::Object(map);
        }
        // The portal realm's user profile requires firstName AND lastName: a user created without either is
        // sent through VERIFY_PROFILE ("update your account") at first login.
        if let Some(n) = first_name.filter(|n| !n.is_empty()) {
            rep["firstName"] = json!(n);
        }
        if let Some(n) = last_name.filter(|n| !n.is_empty()) {
            rep["lastName"] = json!(n);
        }
        let what = "keycloak create user";
        let resp = self
            .send(
                self.http.post(self.base.endpoint(&["users"])).json(&rep),
                what,
            )
            .await?;
        if resp.status() == StatusCode::CONFLICT {
            return Ok(None);
        }
        if resp.status() != StatusCode::CREATED {
            return Err(UpstreamError::status(what, resp.status()));
        }
        resp.headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| l.trim_end_matches('/').rsplit('/').next())
            .filter(|id| !id.is_empty())
            .map(|id| Some(id.to_string()))
            .ok_or_else(|| UpstreamError::new(None, "keycloak create user: no id in Location"))
    }

    /// Merge `attrs` into the user's attributes (`None` removes one). Read-modify-write of the whole
    /// representation, because Keycloak's `PUT /users/{id}` replaces the attribute map: nothing else the user
    /// holds (other apps' attributes) is lost.
    pub async fn update_attributes(
        &self,
        id: &str,
        attrs: &[(&str, Option<&str>)],
    ) -> Result<(), UpstreamError> {
        let url = self.base.endpoint(&["users", id]);
        let what = "keycloak read user";
        let resp = self.send(self.http.get(url.clone()), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let mut rep: Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let mut map = rep
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        for (k, v) in attrs {
            match v {
                Some(v) => {
                    map.insert((*k).to_string(), json!([v]));
                }
                None => {
                    map.remove(*k);
                }
            }
        }
        rep["attributes"] = Value::Object(map);
        let what = "keycloak update user attributes";
        let resp = self.send(self.http.put(url).json(&rep), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(())
    }

    /// Fill an EMPTY firstName / lastName on `id` (commonpractice `backfillUserNames`): a reused identity without
    /// both names is sent through VERIFY_PROFILE at first sign-in, an extra screen before the set-password one. A name
    /// someone already set is never overwritten. Read-modify-write of the whole representation, so no attribute is
    /// lost. `Ok(false)` when nothing was missing (or nothing given to fill it with): no PUT.
    pub async fn backfill_names(
        &self,
        id: &str,
        first_name: Option<&str>,
        last_name: Option<&str>,
    ) -> Result<bool, UpstreamError> {
        let url = self.base.endpoint(&["users", id]);
        let what = "keycloak read user";
        let resp = self.send(self.http.get(url.clone()), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let mut rep: Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let mut changed = false;
        for (key, given) in [("firstName", first_name), ("lastName", last_name)] {
            let empty = rep
                .get(key)
                .and_then(Value::as_str)
                .is_none_or(|v| v.trim().is_empty());
            if let Some(n) = given.map(str::trim).filter(|n| !n.is_empty())
                && empty
            {
                rep[key] = json!(n);
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        let what = "keycloak backfill user names";
        let resp = self.send(self.http.put(url).json(&rep), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(true)
    }

    /// Has `id` ANY credential (of any type) or identity-provider link? `Some(false)` only when Keycloak says so
    /// positively for both; `None` when either list is refused to the service account (commonpractice
    /// `identityCredentialState`: unknown is never "none").
    pub async fn has_credential_or_link(&self, id: &str) -> Result<Option<bool>, UpstreamError> {
        let mut unknown = false;
        for (path, what) in [
            ("credentials", "keycloak read credentials"),
            ("federated-identity", "keycloak read identity links"),
        ] {
            let r = self
                .send(
                    self.http.get(self.base.endpoint(&["users", id, path])),
                    what,
                )
                .await?;
            if r.status() == StatusCode::FORBIDDEN {
                unknown = true;
                continue;
            }
            if !r.status().is_success() {
                return Err(UpstreamError::status(what, r.status()));
            }
            let items: Vec<Value> = r
                .json()
                .await
                .map_err(|e| UpstreamError::transport(what, &e))?;
            if !items.is_empty() {
                return Ok(Some(true));
            }
        }
        Ok(if unknown { None } else { Some(false) })
    }

    /// Re-enable `id` and drop its `attr` attribute when that attribute starts with `prefix` (an identity this
    /// service retired itself, CP `reclaimDisabledIdentity`), in one read-modify-write: the marker it carried, or
    /// `None` (nothing changed) when it carries no such marker.
    pub async fn reclaim_marked(
        &self,
        id: &str,
        attr: &str,
        prefix: &str,
    ) -> Result<Option<String>, UpstreamError> {
        let url = self.base.endpoint(&["users", id]);
        let what = "keycloak read user";
        let resp = self.send(self.http.get(url.clone()), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let mut rep: Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let Some(marker) = rep["attributes"][attr][0]
            .as_str()
            .filter(|v| v.starts_with(prefix))
            .map(str::to_string)
        else {
            return Ok(None);
        };
        let mut map = rep
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        map.remove(attr);
        rep["attributes"] = Value::Object(map);
        rep["enabled"] = json!(true);
        let what = "keycloak re-enable user";
        let resp = self.send(self.http.put(url).json(&rep), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(Some(marker))
    }

    /// The first value of `id`'s attribute `attr`, if any.
    pub async fn attribute(&self, id: &str, attr: &str) -> Result<Option<String>, UpstreamError> {
        let url = self.base.endpoint(&["users", id]);
        let what = "keycloak read user";
        let resp = self.send(self.http.get(url), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let rep: Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        Ok(rep["attributes"][attr][0].as_str().map(str::to_string))
    }

    /// Delete the identity `id`; `false` when it was already gone (404).
    pub async fn delete_user(&self, id: &str) -> Result<bool, UpstreamError> {
        let what = "keycloak delete user";
        let url = self.base.endpoint(&["users", id]);
        let resp = self.send(self.http.delete(url), what).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(true)
    }

    /// Disable `id` and set the attribute `attr` = `value` in ONE read-modify-write of the representation (every
    /// other attribute kept), then read it back: `true` when both the disable and the attribute persisted, `false`
    /// when only the disable did (a user profile that does not declare the attribute drops it). A disable that
    /// did not persist is an error.
    pub async fn disable_and_mark(
        &self,
        id: &str,
        attr: &str,
        value: &str,
    ) -> Result<bool, UpstreamError> {
        let url = self.base.endpoint(&["users", id]);
        let what = "keycloak read user";
        let resp = self.send(self.http.get(url.clone()), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let mut rep: Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let mut map = rep
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        map.insert(attr.to_string(), json!([value]));
        rep["attributes"] = Value::Object(map);
        rep["enabled"] = json!(false);
        let what = "keycloak disable user";
        let resp = self
            .send(self.http.put(url.clone()).json(&rep), what)
            .await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let what = "keycloak read user back";
        let resp = self.send(self.http.get(url.clone()), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let back: Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        if back.get("enabled").and_then(Value::as_bool) != Some(false) {
            return Err(UpstreamError::new(
                None,
                "keycloak disable user: the disable did not persist",
            ));
        }
        Ok(back["attributes"][attr][0].as_str() == Some(value))
    }

    /// Can `id` sign in yet? The user read is fatal on failure. The credentials read may be refused to the
    /// service account (403): then, as in commonpractice, only `requiredActions` decides ("can sign in"
    /// unless `UPDATE_PASSWORD` is pending).
    pub async fn login_state(&self, id: &str) -> Result<LoginState, UpstreamError> {
        let url = self.base.endpoint(&["users", id]);
        let what = "keycloak read user";
        let resp = self.send(self.http.get(url.clone()), what).await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let u: UserState = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let pending_password = u.required_actions.iter().any(|a| a == "UPDATE_PASSWORD");
        let what = "keycloak read credentials";
        let r = self
            .send(
                self.http
                    .get(self.base.endpoint(&["users", id, "credentials"])),
                what,
            )
            .await?;
        let has_password = if r.status().is_success() {
            r.json::<Vec<Credential>>()
                .await
                .ok()
                .map(|cs| cs.iter().any(|c| c.kind.as_deref() == Some("password")))
        } else {
            tracing::warn!(
                status = r.status().as_u16(),
                "keycloak_credentials_unreadable; deciding by requiredActions"
            );
            None
        };
        Ok(LoginState {
            needs_setup: pending_password || has_password == Some(false),
            email_verified: u.email_verified,
        })
    }

    /// The identity `id`; `None` only on Keycloak's 404 (any other failure is an error, never "no such user").
    pub async fn get_user(&self, id: &str) -> Result<Option<PortalUser>, UpstreamError> {
        let what = "keycloak get user";
        let url = self.base.endpoint(&["users", id]);
        let resp = self.send(self.http.get(url), what).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let u: FullUser = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let parts: Vec<&str> = [u.first_name.as_deref(), u.last_name.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let name = if parts.is_empty() {
            u.username
        } else {
            Some(parts.join(" "))
        };
        Ok(Some(PortalUser {
            sub: u.id,
            email: u.email,
            name,
        }))
    }

    /// Does `id` hold `role` on the client whose internal id is `client_uuid` (effective mapping, composites
    /// included)? The id comes from configuration (`AGENT_KC_CLIENT_UUID`): no client lookup, so the service account
    /// needs `view-users` only. An unknown client or user is `false`; any other failure is an error.
    pub async fn has_client_role(
        &self,
        id: &str,
        client_uuid: &str,
        role: &str,
    ) -> Result<bool, UpstreamError> {
        let what = "keycloak read client role mappings";
        let url = self.base.endpoint(&[
            "users",
            id,
            "role-mappings",
            "clients",
            client_uuid,
            "composite",
        ]);
        let resp = self.send(self.http.get(url), what).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let roles: Vec<WireRole> = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        Ok(roles.iter().any(|r| r.name == role))
    }

    /// Keycloak's action email (`to.actions`, e.g. [`INVITE_ACTIONS`]): the link lands the person in the app
    /// (`client_id`, `redirect_uri`) after the actions. No password is ever emailed.
    pub async fn execute_actions_email(
        &self,
        id: &str,
        to: &ActionsEmail<'_>,
    ) -> Result<(), UpstreamError> {
        let what = "keycloak execute-actions-email";
        let lifespan = to.lifespan_seconds.to_string();
        let url = self.base.endpoint_with_query(
            &["users", id, "execute-actions-email"],
            &[
                ("client_id", to.client_id),
                ("redirect_uri", to.redirect_uri),
                ("lifespan", &lifespan),
            ],
        );
        let resp = self
            .send(self.http.put(url).json(&to.actions), what)
            .await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(())
    }
}
