//! One error envelope for every route: `{"error": {"code", "message"}}`. After care-bus-agents@45ee128
//! `crates/carebus-api/src/error.rs`, with strata_configurator's coded `Denied` / `Missing` / `Unavailable`.
use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400: extractor rejections (`invalid_body`, `invalid_query`, `invalid_path`) and `invalid_active_tenant`.
    #[error("{1}")]
    BadRequest(&'static str, String),
    /// 401 `unauthenticated`: no token, or one that does not verify.
    #[error("missing or invalid token")]
    Unauthenticated,
    /// 403 with its machine code: `forbidden`, `super_admin_required`, `not_provisioned`, `tenant_suspended`,
    /// `tenant_archived`, `signup_*_forbidden`, ...
    #[error("{1}")]
    Forbidden(&'static str, String),
    /// 404 `<thing>_not_found` or `not_found`.
    #[error("{1}")]
    Missing(&'static str, String),
    /// 409.
    #[error("{1}")]
    Conflict(&'static str, String),
    /// 422: every validation refusal (spec 16 §3).
    #[error("{1}")]
    Unprocessable(&'static str, String),
    /// 413.
    #[error("{1}")]
    TooLarge(&'static str, String),
    /// 415.
    #[error("{1}")]
    Unsupported(&'static str, String),
    /// 502: an upstream refused (`licences_unavailable`, `keycloak_error`, ...).
    #[error("{1}")]
    BadGateway(&'static str, String),
    /// 503, with `Retry-After: <seconds>` when the third field is set.
    #[error("{1}")]
    Unavailable(&'static str, String, Option<u64>),
    /// Any status with a code and extra fields merged into the error object (`release_not_ready{blocking}`, ...).
    #[error("{message}")]
    Detailed {
        status: StatusCode,
        code: &'static str,
        message: String,
        extra: Map<String, Value>,
    },
    #[error("this route does not accept that method")]
    MethodNotAllowed,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// `RAISE EXCEPTION '<code>'` (SQLSTATE P0001) messages of this crate's definer functions and triggers: such a
/// message is the 409 code itself. Any other P0001 is a 500.
pub const KNOWN_DB_CODES: &[&str] = &[
    "connector_template_not_found",
    "residency_immutable",
    "same_tenant",
    "template_immutable",
    "tenant_archived",
    "tenant_not_active",
    "tenant_not_found",
    "user_active",
    "user_not_found",
];

/// A database error by its SQLSTATE and message.
fn from_db_parts(code: Option<&str>, message: &str) -> Option<ApiError> {
    match code {
        Some("23505") => Some(ApiError::Conflict("conflict", "that already exists".into())),
        Some("P0001") => KNOWN_DB_CODES
            .iter()
            .copied()
            .find(|c| *c == message)
            .map(|c| ApiError::Conflict(c, message.replace('_', " "))),
        _ => None,
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => ApiError::Missing("not_found", "not found".into()),
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => {
                ApiError::Unavailable(
                    "unavailable",
                    "the database is unavailable; try again shortly".into(),
                    None,
                )
            }
            sqlx::Error::Database(d) => {
                let code = d.code();
                from_db_parts(code.as_deref(), d.message())
                    .unwrap_or_else(|| ApiError::Internal(e.into()))
            }
            _ => ApiError::Internal(e.into()),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// HTTP status and machine code only; never carries message text.
    pub fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(c, _) => (StatusCode::BAD_REQUEST, *c),
            ApiError::Unauthenticated => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            ApiError::Forbidden(c, _) => (StatusCode::FORBIDDEN, *c),
            ApiError::Missing(c, _) => (StatusCode::NOT_FOUND, *c),
            ApiError::Conflict(c, _) => (StatusCode::CONFLICT, *c),
            ApiError::Unprocessable(c, _) => (StatusCode::UNPROCESSABLE_ENTITY, *c),
            ApiError::TooLarge(c, _) => (StatusCode::PAYLOAD_TOO_LARGE, *c),
            ApiError::Unsupported(c, _) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, *c),
            ApiError::BadGateway(c, _) => (StatusCode::BAD_GATEWAY, *c),
            ApiError::Unavailable(c, _, _) => (StatusCode::SERVICE_UNAVAILABLE, *c),
            ApiError::Detailed { status, code, .. } => (*status, *code),
            ApiError::MethodNotAllowed => (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }

    /// The machine code alone.
    pub fn code(&self) -> &'static str {
        self.status_and_code().1
    }

    /// A coded error whose error object carries extra fields (add them with [`ApiError::with`]).
    pub fn detailed(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Self {
        ApiError::Detailed {
            status,
            code,
            message: msg.into(),
            extra: Map::new(),
        }
    }

    /// Add `k` to the error object. Meant for [`ApiError::Detailed`]; any other coded error becomes a
    /// `Detailed` with the same status, code and message. `Internal` is returned unchanged (its text never
    /// reaches a response). `code` and `message` cannot be overwritten.
    pub fn with(self, k: &str, v: impl Into<Value>) -> Self {
        match self {
            ApiError::Internal(_) => self,
            ApiError::Detailed {
                status,
                code,
                message,
                mut extra,
            } => {
                extra.insert(k.to_string(), v.into());
                ApiError::Detailed {
                    status,
                    code,
                    message,
                    extra,
                }
            }
            other => {
                let (status, code) = other.status_and_code();
                ApiError::detailed(status, code, other.to_string()).with(k, v)
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(e) = &self {
            tracing::error!(error = %format!("{e:#}"), "internal error");
        }
        let (status, code) = self.status_and_code();
        let message = match &self {
            ApiError::Internal(_) => "Something went wrong on our side.".to_string(),
            o => o.to_string(),
        };
        let retry_after = match &self {
            ApiError::Unavailable(_, _, s) => *s,
            _ => None,
        };
        let mut obj = match self {
            ApiError::Detailed { extra, .. } => extra,
            _ => Map::new(),
        };
        obj.insert("code".into(), json!(code));
        obj.insert("message".into(), json!(message));
        let mut resp = (status, Json(json!({ "error": Value::Object(obj) }))).into_response();
        if let Some(s) = retry_after {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(s),
            );
        }
        resp
    }
}

/// Unknown route: the same error envelope as everything else.
pub async fn fallback() -> ApiError {
    ApiError::Missing("not_found", "route not found".into())
}

/// Known route, wrong method: the same error envelope.
pub async fn method_not_allowed() -> ApiError {
    ApiError::MethodNotAllowed
}

/// `Json<T>` whose rejection is a 400 `invalid_body` envelope. Routes never use bare `Json` input.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(r) => Err(reject_json(r)),
        }
    }
}

fn reject_json(r: JsonRejection) -> ApiError {
    ApiError::BadRequest(
        "invalid_body",
        format!("The request body is not valid: {}", r.body_text()),
    )
}

/// A body taken as `ApiJson<Value>` decoded into `T` once the handler's own role check has passed (a forbidden caller
/// is 403 whatever it sent): 400 `invalid_body` when it does not fit, the envelope [`ApiJson`] answers.
pub fn json_body<T: DeserializeOwned>(raw: Value) -> ApiResult<T> {
    serde_json::from_value(raw).map_err(|e| {
        ApiError::BadRequest(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })
}

/// `Query<T>` whose rejection is a 400 `invalid_query` envelope.
pub struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(r) => Err(reject_query(r)),
        }
    }
}

fn reject_query(r: QueryRejection) -> ApiError {
    ApiError::BadRequest(
        "invalid_query",
        format!("The query string is not valid: {}", r.body_text()),
    )
}

/// `Path<T>` whose rejection is a 400 `invalid_path` envelope.
pub struct ApiPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(v)) => Ok(Self(v)),
            Err(r) => Err(reject_path(r)),
        }
    }
}

fn reject_path(r: PathRejection) -> ApiError {
    ApiError::BadRequest(
        "invalid_path",
        format!("The path is not valid: {}", r.body_text()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Deserialize)]
    struct Body1 {
        #[allow(dead_code)]
        n: i32,
    }
    #[derive(Deserialize)]
    struct Q {
        #[allow(dead_code)]
        limit: i64,
    }

    fn app() -> Router {
        Router::new()
            .route("/body", post(|ApiJson(_): ApiJson<Body1>| async { "ok" }))
            .route("/q", get(|ApiQuery(_): ApiQuery<Q>| async { "ok" }))
            .route("/p/{id}", get(|ApiPath(_): ApiPath<i64>| async { "ok" }))
    }

    async fn code(req: Request<Body>) -> (StatusCode, String) {
        let resp = app().oneshot(req).await.unwrap();
        let st = resp.status();
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
        (st, v["error"]["code"].as_str().unwrap_or("").to_string())
    }

    #[tokio::test]
    async fn extractor_rejections_use_the_envelope() {
        let bad_json = Request::post("/body")
            .header("content-type", "application/json")
            .body(Body::from("{\"n\":\"x\"}"))
            .unwrap();
        assert_eq!(
            code(bad_json).await,
            (StatusCode::BAD_REQUEST, "invalid_body".into())
        );
        let bad_q = Request::get("/q?limit=abc").body(Body::empty()).unwrap();
        assert_eq!(
            code(bad_q).await,
            (StatusCode::BAD_REQUEST, "invalid_query".into())
        );
        let bad_p = Request::get("/p/abc").body(Body::empty()).unwrap();
        assert_eq!(
            code(bad_p).await,
            (StatusCode::BAD_REQUEST, "invalid_path".into())
        );
        let ok = Request::get("/p/7").body(Body::empty()).unwrap();
        assert_eq!(code(ok).await.0, StatusCode::OK);
    }

    async fn body_of(e: ApiError) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let resp = e.into_response();
        let st = resp.status();
        let h = resp.headers().clone();
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        (st, h, serde_json::from_slice(&b).unwrap())
    }

    #[tokio::test]
    async fn every_variant_has_its_status_and_code() {
        let m = || "m".to_string();
        let cases = vec![
            (
                ApiError::BadRequest("invalid_body", m()),
                400,
                "invalid_body",
            ),
            (ApiError::Unauthenticated, 401, "unauthenticated"),
            (
                ApiError::Forbidden("super_admin_required", m()),
                403,
                "super_admin_required",
            ),
            (
                ApiError::Missing("tenant_not_found", m()),
                404,
                "tenant_not_found",
            ),
            (ApiError::Conflict("job_exists", m()), 409, "job_exists"),
            (
                ApiError::Unprocessable("invalid_name", m()),
                422,
                "invalid_name",
            ),
            (ApiError::TooLarge("too_large", m()), 413, "too_large"),
            (
                ApiError::Unsupported("unsupported_media_type", m()),
                415,
                "unsupported_media_type",
            ),
            (
                ApiError::BadGateway("keycloak_error", m()),
                502,
                "keycloak_error",
            ),
            (
                ApiError::Unavailable("unavailable", m(), None),
                503,
                "unavailable",
            ),
            (
                ApiError::detailed(StatusCode::CONFLICT, "release_not_ready", m()),
                409,
                "release_not_ready",
            ),
            (ApiError::MethodNotAllowed, 405, "method_not_allowed"),
            (ApiError::Internal(anyhow::anyhow!("x")), 500, "internal"),
        ];
        for (e, status, code) in cases {
            assert_eq!(
                e.status_and_code(),
                (StatusCode::from_u16(status).unwrap(), code)
            );
            let (st, _, b) = body_of(e).await;
            assert_eq!(st.as_u16(), status, "{code}");
            assert_eq!(b["error"]["code"], code);
            assert!(b["error"]["message"].is_string(), "{code}: {b}");
        }
        // sqlx pool/IO failures are a coded 503, not a 500
        let e: ApiError = sqlx::Error::PoolTimedOut.into();
        assert_eq!(
            e.status_and_code(),
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        );
        let e: ApiError = sqlx::Error::RowNotFound.into();
        assert_eq!(e.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
    }

    #[tokio::test]
    async fn detailed_extras_are_merged_into_the_error_object() {
        let e = ApiError::detailed(StatusCode::CONFLICT, "owned_by_active_run", "busy")
            .with("run_id", "r-1")
            .with("blocking", serde_json::json!(["a", "b"]))
            // cannot overwrite the code or the message
            .with("code", "evil")
            .with("message", "evil");
        let (st, _, b) = body_of(e).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(
            b,
            serde_json::json!({"error": {"code": "owned_by_active_run", "message": "busy",
                "run_id": "r-1", "blocking": ["a", "b"]}})
        );
        // `with` on another coded error keeps its status and code
        let (st, _, b) = body_of(ApiError::Conflict("x_taken", "taken".into()).with("n", 2)).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(b["error"]["code"], "x_taken");
        assert_eq!(b["error"]["n"], 2);
    }

    #[tokio::test]
    async fn unavailable_with_seconds_sets_retry_after() {
        let (st, h, b) = body_of(ApiError::Unavailable(
            "signup_queue_full",
            "x".into(),
            Some(30),
        ))
        .await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(h.get("retry-after").unwrap(), "30");
        assert_eq!(b["error"]["code"], "signup_queue_full");
        let (_, h, _) = body_of(ApiError::Unavailable("unavailable", "x".into(), None)).await;
        assert!(h.get("retry-after").is_none());
    }

    #[test]
    fn p0001_known_message_maps_to_conflict_code() {
        let e = from_db_parts(Some("P0001"), "tenant_archived").unwrap();
        assert_eq!(
            e.status_and_code(),
            (StatusCode::CONFLICT, "tenant_archived")
        );
        for c in KNOWN_DB_CODES {
            assert_eq!(from_db_parts(Some("P0001"), c).unwrap().code(), *c);
        }
        // an unknown RAISE is not a client error
        assert!(from_db_parts(Some("P0001"), "something else").is_none());
        let e = from_db_parts(Some("23505"), "duplicate key").unwrap();
        assert_eq!(e.status_and_code(), (StatusCode::CONFLICT, "conflict"));
        assert!(from_db_parts(Some("42501"), "permission denied").is_none());
        let mut sorted = KNOWN_DB_CODES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KNOWN_DB_CODES, "keep KNOWN_DB_CODES sorted");
    }

    #[tokio::test]
    async fn unknown_db_error_is_500_without_text() {
        let e: ApiError =
            sqlx::Error::Protocol("relation secret_table leaked detail".into()).into();
        assert!(matches!(e, ApiError::Internal(_)));
        let (st, _, b) = body_of(e).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(b["error"]["code"], "internal");
        let text = b.to_string();
        assert!(
            !text.contains("secret_table") && !text.contains("leaked"),
            "{text}"
        );
    }
}
