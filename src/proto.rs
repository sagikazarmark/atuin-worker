//! The protocol layer over `http` types: settings from the Worker's vars, routing, capability
//! negotiation, the fixed-answer routes, JSON error bodies, and the wrapper that adds the protocol
//! headers to every response.
//!
//! Every string here was checked against upstream `atuinsh/atuin` at v18.24.4.

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Response, StatusCode};
use serde::Serialize;

use crate::caps::{AVAILABLE_HEADER, Capabilities, ENFORCE_HEADER, KNOWN_HEADER};
use crate::wire::{ErrorResponse, HealthResponse, IndexResponse, MeResponse};

/// The upstream Atuin version this build tracks, advertised in `Atuin-Version` and `GET /`.
pub const ATUIN_VERSION: &str = "18.24.4";
/// Default for the `PAGE_SIZE` var.
pub const DEFAULT_PAGE_SIZE: u64 = 100;
/// Default for the `MAX_RECORD_SIZE` var: 256 KiB.
pub const DEFAULT_MAX_RECORD_SIZE: u64 = 256 * 1024;
/// Upper bound on `PAGE_SIZE × MAX_RECORD_SIZE`. D1 refuses query results over 50 MB, and a page
/// of records carries more than `data`; 40 MiB leaves room for the rest of each row.
pub const MAX_PAGE_BYTES: u64 = 40 * 1024 * 1024;

pub const VERSION_HEADER: &str = "atuin-version";
pub const CLACKS_HEADER: &str = "x-clacks-overhead";
pub const CLACKS_VALUE: &str = "GNU Terry Pratchett, Kris Nova";

const HOMAGE: &str = r#""Through the fathomless deeps of space swims the star turtle Great A'Tuin, bearing on its back the four giant elephants who carry on their shoulders the mass of the Discworld." -- Sir Terry Pratchett"#;

/// Error reasons, as clients display them.
pub mod reason {
    pub const USER_NOT_FOUND: &str = "user not found";
    pub const REGISTRATION_CLOSED: &str = "this server is not open for registrations";
    pub const PASSWORD_MANAGED: &str =
        "password is managed by the server operator; change the ATUIN_PASSWORD secret";
    pub const ACCOUNT_MANAGED: &str = "account is managed by the server operator; to replace the server's data with this machine's, use atuin store push --force";
    pub const INVALID_BODY: &str = "invalid request body";
    pub const RECORD_TOO_LARGE: &str = "could not add records; record too large";
    pub const ADD_RECORD_FAILED: &str = "failed to add record";
    pub const INDEX_FAILED: &str = "failed to calculate record index";
    pub const DELETE_STORE_FAILED: &str = "failed to delete store";
    pub const TOO_MANY_LOGINS: &str = "too many login attempts";
    pub const RATE_LIMIT_FAILED: &str = "could not check the login rate limit";
    pub const LIMITER_MISSING: &str = "server not configured: LOGIN_LIMIT binding missing";
    pub const DB_MISSING: &str = "server not configured: DB binding missing";
}

/// The vars of the Worker, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Records per page (`PAGE_SIZE`).
    pub page_size: u64,
    /// Largest accepted `data` field of a record, in bytes (`MAX_RECORD_SIZE`).
    pub max_record_size: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            max_record_size: DEFAULT_MAX_RECORD_SIZE,
        }
    }
}

impl Settings {
    /// Validate the vars. Absent vars take their defaults; the error is the part of a
    /// `server not configured: …` reason that names the problem.
    pub fn from_vars(
        page_size: Option<&str>,
        max_record_size: Option<&str>,
    ) -> Result<Self, String> {
        let defaults = Self::default();

        let page_size = positive(page_size, "PAGE_SIZE")?.unwrap_or(defaults.page_size);
        let max_record_size =
            positive(max_record_size, "MAX_RECORD_SIZE")?.unwrap_or(defaults.max_record_size);

        if page_size.saturating_mul(max_record_size) > MAX_PAGE_BYTES {
            return Err(format!(
                "PAGE_SIZE × MAX_RECORD_SIZE must not exceed {MAX_PAGE_BYTES} bytes (D1 limits \
                 query results to 50 MB)"
            ));
        }

        Ok(Self {
            page_size,
            max_record_size,
        })
    }
}

fn positive(value: Option<&str>, name: &str) -> Result<Option<u64>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(format!("{name} must be a positive integer")),
        },
    }
}

/// The thirteen upstream routes, plus the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Healthz,
    Capabilities,
    Index,
    GetUser,
    Register,
    Login,
    Me,
    ChangePassword,
    DeleteAccount,
    AddRecords,
    RecordStatus,
    NextRecords,
    DeleteStore,
    NotFound,
}

impl Route {
    /// Match a request's method and path to a route. Paths match exactly, as upstream's axum
    /// router; anything else, including a known path with another method, is [`Route::NotFound`].
    pub fn recognize(method: &Method, path: &str) -> Self {
        match (method.as_str(), path) {
            ("GET", "/healthz") => Self::Healthz,
            ("GET", "/api/v0/capabilities") => Self::Capabilities,
            ("GET", "/") => Self::Index,
            ("POST", "/register") => Self::Register,
            ("POST", "/login") => Self::Login,
            ("GET", "/api/v0/me") => Self::Me,
            ("PATCH", "/account/password") => Self::ChangePassword,
            ("DELETE", "/account") => Self::DeleteAccount,
            ("POST", "/api/v0/record") => Self::AddRecords,
            ("GET", "/api/v0/record") => Self::RecordStatus,
            ("GET", "/api/v0/record/next") => Self::NextRecords,
            ("DELETE", "/api/v0/store") => Self::DeleteStore,
            ("GET", p) => match p.strip_prefix("/user/") {
                Some(name) if !name.is_empty() && !name.contains('/') => Self::GetUser,
                _ => Self::NotFound,
            },
            _ => Self::NotFound,
        }
    }

    /// The route's pattern, for logs. Never the raw path: `/user/{username}` would log a name.
    pub fn pattern(self) -> &'static str {
        match self {
            Self::Healthz => "GET /healthz",
            Self::Capabilities => "GET /api/v0/capabilities",
            Self::Index => "GET /",
            Self::GetUser => "GET /user/{username}",
            Self::Register => "POST /register",
            Self::Login => "POST /login",
            Self::Me => "GET /api/v0/me",
            Self::ChangePassword => "PATCH /account/password",
            Self::DeleteAccount => "DELETE /account",
            Self::AddRecords => "POST /api/v0/record",
            Self::RecordStatus => "GET /api/v0/record",
            Self::NextRecords => "GET /api/v0/record/next",
            Self::DeleteStore => "DELETE /api/v0/store",
            Self::NotFound => "fallback",
        }
    }

    /// Whether the route runs behind the config check and capability negotiation. `/healthz`,
    /// the capability document and the 404 fallback do not.
    pub fn is_checked(self) -> bool {
        !matches!(self, Self::Healthz | Self::Capabilities | Self::NotFound)
    }

    /// Whether the route requires `Authorization: Token <session>`.
    pub fn needs_token(self) -> bool {
        matches!(
            self,
            Self::Me
                | Self::ChangePassword
                | Self::DeleteAccount
                | Self::AddRecords
                | Self::RecordStatus
                | Self::NextRecords
                | Self::DeleteStore
        )
    }
}

/// The outcome of capability negotiation (upstream `caps::axum::check_token`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiation {
    /// No token, a matching token, or a token that is not UTF-8: serve the request.
    Current,
    /// A differing token without `x-atuin-capabilities-enforce`: serve the request, and tell the
    /// client our token.
    Stale,
    /// A differing token with `x-atuin-capabilities-enforce`: answer 412.
    Rejected,
}

/// Compare the client's `x-atuin-capabilities-known` with our token.
pub fn negotiate(headers: &HeaderMap, token: &str) -> Negotiation {
    match headers.get(KNOWN_HEADER).and_then(|v| v.to_str().ok()) {
        Some(known) if known != token => {
            if headers.contains_key(ENFORCE_HEADER) {
                Negotiation::Rejected
            } else {
                Negotiation::Stale
            }
        }
        _ => Negotiation::Current,
    }
}

/// A JSON response with `Content-Type: application/json`.
pub fn json<T: Serialize>(status: StatusCode, value: &T) -> Response<Bytes> {
    let body = serde_json::to_vec(value).expect("response types serialize");
    let mut resp = Response::new(Bytes::from(body));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// A plain-text response, as axum answers a `(StatusCode, &str)`.
fn text(status: StatusCode, body: &'static str) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from_static(body.as_bytes()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// An error answer that has not been rendered yet: a status and one of the fixed reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejection {
    pub status: StatusCode,
    pub reason: &'static str,
}

impl Rejection {
    pub const fn new(status: StatusCode, reason: &'static str) -> Self {
        Self { status, reason }
    }

    /// Render as `{"reason": "…"}`.
    pub fn into_response(self) -> Response<Bytes> {
        error(self.status, self.reason)
    }
}

/// `{"reason": "…"}` with the given status.
pub fn error(status: StatusCode, reason: impl Into<String>) -> Response<Bytes> {
    json(
        status,
        &ErrorResponse {
            reason: reason.into(),
        },
    )
}

/// A 500 for a configuration problem: `server not configured: <problem>`. 500 and not 503,
/// because the client replaces any 503 with a generic status-page message and drops the reason.
pub fn not_configured(problem: &str) -> Response<Bytes> {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("server not configured: {problem}"),
    )
}

/// An empty 200, as upstream's handlers that return `()`.
pub fn empty_ok() -> Response<Bytes> {
    Response::new(Bytes::new())
}

/// The fallback for unknown routes: plain-text `404 not found`.
pub fn not_found() -> Response<Bytes> {
    text(StatusCode::NOT_FOUND, "404 not found")
}

/// The answer to a stale token with enforcement: plain-text 412.
pub fn precondition_failed() -> Response<Bytes> {
    text(StatusCode::PRECONDITION_FAILED, "capabilities out of date")
}

/// `GET /healthz`.
pub fn healthz() -> Response<Bytes> {
    json(
        StatusCode::OK,
        &HealthResponse {
            status: "healthy".to_owned(),
        },
    )
}

/// `GET /api/v0/capabilities`: the document, served verbatim.
pub fn capabilities(caps: &Capabilities) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from(caps.body().to_owned()));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// `GET /`.
pub fn index() -> Response<Bytes> {
    json(
        StatusCode::OK,
        &IndexResponse {
            homage: HOMAGE.to_owned(),
            version: ATUIN_VERSION.to_owned(),
        },
    )
}

/// `GET /user/{username}`: always 404, even for the configured name, so it cannot be probed.
pub fn get_user() -> Response<Bytes> {
    error(StatusCode::NOT_FOUND, reason::USER_NOT_FOUND)
}

/// `POST /register`: refused.
pub fn register() -> Response<Bytes> {
    error(StatusCode::BAD_REQUEST, reason::REGISTRATION_CLOSED)
}

/// `GET /api/v0/me`.
pub fn me(username: &str) -> Response<Bytes> {
    json(
        StatusCode::OK,
        &MeResponse {
            username: username.to_owned(),
        },
    )
}

/// `PATCH /account/password`: refused.
pub fn change_password() -> Response<Bytes> {
    error(StatusCode::BAD_REQUEST, reason::PASSWORD_MANAGED)
}

/// `DELETE /account`: refused.
pub fn delete_account() -> Response<Bytes> {
    error(StatusCode::BAD_REQUEST, reason::ACCOUNT_MANAGED)
}

/// Add the protocol headers to a finished response: `Atuin-Version` and `X-Clacks-Overhead` on
/// every response, and `x-atuin-capabilities-available` when the client's token was stale.
pub fn finish(resp: &mut Response<Bytes>, negotiation: Negotiation, caps: &Capabilities) {
    let headers = resp.headers_mut();
    headers.insert(
        HeaderName::from_static(VERSION_HEADER),
        HeaderValue::from_static(ATUIN_VERSION),
    );
    headers.insert(
        HeaderName::from_static(CLACKS_HEADER),
        HeaderValue::from_static(CLACKS_VALUE),
    );
    if negotiation != Negotiation::Current
        && let Ok(token) = HeaderValue::from_str(caps.token())
    {
        headers.insert(HeaderName::from_static(AVAILABLE_HEADER), token);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn body_json(resp: &Response<Bytes>) -> Value {
        serde_json::from_slice(resp.body()).unwrap()
    }

    fn reason_of(resp: &Response<Bytes>) -> String {
        body_json(resp)["reason"].as_str().unwrap().to_owned()
    }

    fn headers(pairs: &[(&'static str, &[u8])]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_bytes(value).unwrap());
        }
        map
    }

    const TOKEN: &str = "0123456789abcdef";

    #[test]
    fn negotiation_passes_an_absent_token() {
        assert_eq!(negotiate(&HeaderMap::new(), TOKEN), Negotiation::Current);
        assert_eq!(
            negotiate(&headers(&[(ENFORCE_HEADER, b"1")]), TOKEN),
            Negotiation::Current
        );
    }

    #[test]
    fn negotiation_passes_a_matching_token() {
        let h = headers(&[(KNOWN_HEADER, TOKEN.as_bytes()), (ENFORCE_HEADER, b"1")]);
        assert_eq!(negotiate(&h, TOKEN), Negotiation::Current);
    }

    #[test]
    fn negotiation_serves_a_stale_token_without_enforcement() {
        let h = headers(&[(KNOWN_HEADER, b"deadbeefdeadbeef")]);
        assert_eq!(negotiate(&h, TOKEN), Negotiation::Stale);
    }

    #[test]
    fn negotiation_rejects_a_stale_token_with_enforcement_of_any_value() {
        for value in [&b"1"[..], b"", b"false"] {
            let h = headers(&[(KNOWN_HEADER, b"deadbeefdeadbeef"), (ENFORCE_HEADER, value)]);
            assert_eq!(negotiate(&h, TOKEN), Negotiation::Rejected);
        }
    }

    #[test]
    fn negotiation_treats_a_non_utf8_token_as_absent() {
        let h = headers(&[(KNOWN_HEADER, &[0xff, 0xfe]), (ENFORCE_HEADER, b"1")]);
        assert_eq!(negotiate(&h, TOKEN), Negotiation::Current);
    }

    #[test]
    fn finish_adds_the_protocol_headers_to_any_response() {
        let caps = Capabilities::new(100);
        for mut resp in [
            healthz(),
            not_found(),
            precondition_failed(),
            empty_ok(),
            error(StatusCode::INTERNAL_SERVER_ERROR, "x"),
        ] {
            finish(&mut resp, Negotiation::Current, &caps);
            assert_eq!(resp.headers()["atuin-version"], "18.24.4");
            assert_eq!(
                resp.headers()["x-clacks-overhead"],
                "GNU Terry Pratchett, Kris Nova"
            );
            assert!(resp.headers().get(AVAILABLE_HEADER).is_none());
        }
    }

    #[test]
    fn finish_advertises_our_token_to_a_stale_client() {
        let caps = Capabilities::new(100);
        for negotiation in [Negotiation::Stale, Negotiation::Rejected] {
            let mut resp = precondition_failed();
            finish(&mut resp, negotiation, &caps);
            assert_eq!(resp.headers()[AVAILABLE_HEADER], caps.token());
        }
    }

    #[test]
    fn index_advertises_the_tracked_version() {
        let resp = index();
        assert_eq!(body_json(&resp)["version"], "18.24.4");
        assert!(
            body_json(&resp)["homage"]
                .as_str()
                .unwrap()
                .contains("Great A'Tuin")
        );
    }

    #[test]
    fn settings_default_when_vars_are_absent() {
        assert_eq!(
            Settings::from_vars(None, None).unwrap(),
            Settings::default()
        );
        assert_eq!(
            Settings::from_vars(Some(" "), Some("")).unwrap(),
            Settings::default()
        );
        let s = Settings::from_vars(Some("50"), Some("1024")).unwrap();
        assert_eq!((s.page_size, s.max_record_size), (50, 1024));
    }

    #[test]
    fn settings_reject_bad_vars() {
        for (page, max) in [
            (Some("0"), None),
            (Some("-1"), None),
            (Some("ten"), None),
            (None, Some("0")),
            (Some("1000"), Some("1048576")),
        ] {
            assert!(Settings::from_vars(page, max).is_err(), "{page:?} {max:?}");
        }
    }

    #[test]
    fn error_bodies_carry_upstream_strings() {
        let cases = [
            (get_user(), StatusCode::NOT_FOUND, "user not found"),
            (
                register(),
                StatusCode::BAD_REQUEST,
                "this server is not open for registrations",
            ),
            (
                change_password(),
                StatusCode::BAD_REQUEST,
                "password is managed by the server operator; change the ATUIN_PASSWORD secret",
            ),
            (
                delete_account(),
                StatusCode::BAD_REQUEST,
                "account is managed by the server operator; to replace the server's data with \
                 this machine's, use atuin store push --force",
            ),
        ];
        for (resp, status, reason) in cases {
            assert_eq!(resp.status(), status);
            assert_eq!(resp.headers()[CONTENT_TYPE], "application/json");
            assert_eq!(reason_of(&resp), reason);
        }
        let resp = not_configured("ATUIN_PASSWORD is not set");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            reason_of(&resp),
            "server not configured: ATUIN_PASSWORD is not set"
        );
    }

    #[test]
    fn fallback_and_412_are_plain_text() {
        let resp = not_found();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.body().as_ref(), b"404 not found");
        assert_eq!(resp.headers()[CONTENT_TYPE], "text/plain; charset=utf-8");

        let resp = precondition_failed();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(resp.body().as_ref(), b"capabilities out of date");
    }

    #[test]
    fn fixed_answers() {
        assert_eq!(
            body_json(&healthz()),
            serde_json::json!({"status": "healthy"})
        );
        assert_eq!(
            body_json(&me("alice")),
            serde_json::json!({"username": "alice"})
        );
        let caps = Capabilities::new(100);
        let resp = capabilities(&caps);
        assert_eq!(resp.body().as_ref(), caps.body().as_bytes());
        assert_eq!(resp.headers()[CONTENT_TYPE], "application/json");
        assert!(empty_ok().body().is_empty());
    }

    #[test]
    fn routes_are_recognized_by_method_and_exact_path() {
        use Route::*;
        let cases = [
            ("GET", "/healthz", Healthz),
            ("GET", "/api/v0/capabilities", Capabilities),
            ("GET", "/", Index),
            ("GET", "/user/alice", GetUser),
            ("POST", "/register", Register),
            ("POST", "/login", Login),
            ("GET", "/api/v0/me", Me),
            ("PATCH", "/account/password", ChangePassword),
            ("DELETE", "/account", DeleteAccount),
            ("POST", "/api/v0/record", AddRecords),
            ("GET", "/api/v0/record", RecordStatus),
            ("GET", "/api/v0/record/next", NextRecords),
            ("DELETE", "/api/v0/store", DeleteStore),
            ("GET", "/user/", NotFound),
            ("GET", "/user/a/b", NotFound),
            ("GET", "/healthz/", NotFound),
            ("POST", "/healthz", NotFound),
            ("GET", "/login", NotFound),
            ("PUT", "/api/v0/record", NotFound),
            ("GET", "/api/v0/packfiles", NotFound),
        ];
        for (method, path, route) in cases {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            assert_eq!(Route::recognize(&method, path), route, "{method} {path}");
        }
    }

    #[test]
    fn checks_and_tokens_apply_to_the_right_routes() {
        assert!(!Route::Healthz.is_checked());
        assert!(!Route::Capabilities.is_checked());
        assert!(!Route::NotFound.is_checked());
        assert!(Route::Login.is_checked() && !Route::Login.needs_token());
        assert!(Route::Index.is_checked() && !Route::Index.needs_token());
        assert!(Route::DeleteStore.needs_token());
        assert_eq!(Route::GetUser.pattern(), "GET /user/{username}");
    }
}
