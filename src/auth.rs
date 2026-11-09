//! Authentication for exactly one account, defined by two secrets.
//!
//! There is no auth state anywhere: the session token is an HMAC of the username keyed with the
//! password, so it is computed rather than stored, and rotating `ATUIN_PASSWORD` invalidates it.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http::header::{AUTHORIZATION, HeaderMap};
use http::{Response, StatusCode};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::proto::{self, Rejection};
use crate::wire::{LoginRequest, LoginResponse};

/// Domain separation for the session token, versioned so the derivation can change.
const TOKEN_CONTEXT: &[u8] = b"atuin-worker/session/v1\0";
/// The shortest accepted `ATUIN_PASSWORD`, in characters. There is no slow hash in front of the
/// password, so it has to be long and random.
pub const MIN_PASSWORD_CHARS: usize = 16;

pub mod reason {
    pub const MISSING_HEADER: &str = "missing authorization header";
    pub const INVALID_HEADER: &str = "invalid authorization header encoding";
    pub const SESSION_NOT_FOUND: &str = "session not found";
    pub const WRONG_PASSWORD: &str = "password is not correct";
}

/// The account, checked and with its session token derived. Built once per isolate.
pub struct Credentials {
    username: String,
    token: String,
    username_digest: [u8; 32],
    password_digest: [u8; 32],
    token_digest: [u8; 32],
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

impl Credentials {
    /// The config check: both secrets set, the username in upstream's alphabet
    /// (`[A-Za-z0-9-]+`) and the password at least [`MIN_PASSWORD_CHARS`] long. The error names
    /// the problem for a `server not configured: …` reason and never includes a secret.
    pub fn from_secrets(username: Option<&str>, password: Option<&str>) -> Result<Self, String> {
        let username = match username {
            None | Some("") => return Err("ATUIN_USERNAME is not set".to_owned()),
            Some(u) => u,
        };
        if !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(
                "ATUIN_USERNAME may only contain letters, digits and hyphens (-)".to_owned(),
            );
        }
        let password = match password {
            None | Some("") => return Err("ATUIN_PASSWORD is not set".to_owned()),
            Some(p) => p,
        };
        if password.chars().count() < MIN_PASSWORD_CHARS {
            return Err(format!(
                "ATUIN_PASSWORD must be at least {MIN_PASSWORD_CHARS} characters"
            ));
        }

        let token = derive_token(username, password);
        Ok(Self {
            username: username.to_owned(),
            username_digest: sha256(username.as_bytes()),
            password_digest: sha256(password.as_bytes()),
            token_digest: sha256(token.as_bytes()),
            token,
        })
    }

    /// The configured username, for `GET /api/v0/me`.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Check `Authorization: Token <session>` against the derived token, with upstream's
    /// statuses and reasons. The token comparison is constant-time.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), Rejection> {
        let invalid = || Rejection::new(StatusCode::BAD_REQUEST, reason::INVALID_HEADER);

        let header = headers
            .get(AUTHORIZATION)
            .ok_or_else(|| Rejection::new(StatusCode::BAD_REQUEST, reason::MISSING_HEADER))?;
        let header = header.to_str().map_err(|_| invalid())?;
        let (scheme, token) = header.split_once(' ').ok_or_else(invalid)?;
        if scheme != "Token" {
            return Err(invalid());
        }

        if bool::from(sha256(token.as_bytes()).ct_eq(&self.token_digest)) {
            Ok(())
        } else {
            Err(Rejection::new(
                StatusCode::FORBIDDEN,
                reason::SESSION_NOT_FOUND,
            ))
        }
    }

    /// `POST /login`, after the rate limit: parse the body, compare both credentials in constant
    /// time, and answer with the session token or a uniform 401.
    pub fn login(&self, body: &[u8]) -> Response<Bytes> {
        let Ok(request) = serde_json::from_slice::<LoginRequest>(body) else {
            return proto::error(StatusCode::BAD_REQUEST, proto::reason::INVALID_BODY);
        };

        // Hashing first makes both comparisons fixed-length, so neither length leaks; `&` rather
        // than `&&` so a wrong username does not skip the password comparison.
        let username_ok = sha256(request.username.as_bytes()).ct_eq(&self.username_digest);
        let password_ok = sha256(request.password.as_bytes()).ct_eq(&self.password_digest);

        if bool::from(username_ok & password_ok) {
            proto::json(
                StatusCode::OK,
                &LoginResponse {
                    session: self.token.clone(),
                    auth: Some("cli".to_owned()),
                },
            )
        } else {
            proto::error(StatusCode::UNAUTHORIZED, reason::WRONG_PASSWORD)
        }
    }
}

/// `base64url_nopad(HMAC-SHA256(key = password, msg = "atuin-worker/session/v1\0" + username))`.
pub fn derive_token(username: &str, password: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(password.as_bytes()).expect("HMAC accepts any key length");
    mac.update(TOKEN_CONTEXT);
    mac.update(username.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use serde_json::Value;

    use super::*;

    const USER: &str = "alice";
    const PASS: &str = "correct-horse-battery-staple";
    /// Computed independently: Python `hmac.new(PASS, b"atuin-worker/session/v1\0alice",
    /// sha256)`, base64url without padding.
    const TOKEN: &str = "WjFLDuJPCBiedHAH_xtd7ndNIVn2cRx2xQq6LWHBUzw";

    fn creds() -> Credentials {
        Credentials::from_secrets(Some(USER), Some(PASS)).unwrap()
    }

    fn reason_of(resp: &Response<Bytes>) -> String {
        let v: Value = serde_json::from_slice(resp.body()).unwrap();
        v["reason"].as_str().unwrap().to_owned()
    }

    fn auth(value: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_bytes(value).unwrap());
        h
    }

    #[test]
    fn token_matches_the_fixed_vector() {
        assert_eq!(derive_token(USER, PASS), TOKEN);
        assert_eq!(TOKEN.len(), 43);
    }

    #[test]
    fn token_changes_with_either_secret() {
        assert_ne!(derive_token("bob", PASS), TOKEN);
        assert_ne!(derive_token(USER, "correct-horse-battery-stapler"), TOKEN);
    }

    #[test]
    fn config_check_failures() {
        let long = "0123456789abcdef";
        let cases = [
            (None, Some(long), "ATUIN_USERNAME is not set"),
            (Some(""), Some(long), "ATUIN_USERNAME is not set"),
            (
                Some("al ice"),
                Some(long),
                "ATUIN_USERNAME may only contain letters, digits and hyphens (-)",
            ),
            (
                Some("al_ice"),
                Some(long),
                "ATUIN_USERNAME may only contain letters, digits and hyphens (-)",
            ),
            (
                Some("ålice"),
                Some(long),
                "ATUIN_USERNAME may only contain letters, digits and hyphens (-)",
            ),
            (Some(USER), None, "ATUIN_PASSWORD is not set"),
            (Some(USER), Some(""), "ATUIN_PASSWORD is not set"),
            (
                Some(USER),
                Some("0123456789abcde"),
                "ATUIN_PASSWORD must be at least 16 characters",
            ),
        ];
        for (u, p, expected) in cases {
            assert_eq!(
                Credentials::from_secrets(u, p).unwrap_err(),
                expected,
                "{u:?}"
            );
        }
        assert!(Credentials::from_secrets(Some("Alice-2"), Some(long)).is_ok());
        // Characters, not bytes: 16 two-byte characters pass, 15 do not.
        assert!(Credentials::from_secrets(Some(USER), Some(&"é".repeat(16))).is_ok());
        assert!(Credentials::from_secrets(Some(USER), Some(&"é".repeat(15))).is_err());
    }

    #[test]
    fn debug_never_shows_a_secret() {
        let shown = format!("{:?}", creds());
        assert!(!shown.contains(PASS) && !shown.contains(TOKEN) && !shown.contains(USER));
    }

    #[test]
    fn authorization_accepts_the_derived_token() {
        assert!(
            creds()
                .authorize(&auth(format!("Token {TOKEN}").as_bytes()))
                .is_ok()
        );
    }

    #[test]
    fn authorization_errors() {
        let c = creds();
        let missing = c.authorize(&HeaderMap::new()).unwrap_err();
        assert_eq!(missing.status, StatusCode::BAD_REQUEST);
        assert_eq!(missing.reason, "missing authorization header");
        assert_eq!(
            reason_of(&missing.into_response()),
            "missing authorization header"
        );

        for value in [
            &[0xc3, 0xa9][..], // not visible ASCII
            b"TokenWithoutSpace",
            format!("Bearer {TOKEN}").as_bytes(),
            format!("token {TOKEN}").as_bytes(),
        ] {
            let err = c.authorize(&auth(value)).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{value:?}");
            assert_eq!(err.reason, "invalid authorization header encoding");
        }

        for value in [
            "Token wrong".to_owned(),
            "Token ".to_owned(),
            format!("Token {TOKEN} "),
            format!("Token  {TOKEN}"),
        ] {
            let err = c.authorize(&auth(value.as_bytes())).unwrap_err();
            assert_eq!(err.status, StatusCode::FORBIDDEN, "{value:?}");
            assert_eq!(err.reason, "session not found");
        }
    }

    #[test]
    fn login_returns_the_token_on_a_match() {
        let resp = creds().login(br#"{"username":"alice","password":"correct-horse-battery-staple","totp_code":"123456"}"#);
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = serde_json::from_slice(resp.body()).unwrap();
        assert_eq!(v, serde_json::json!({"session": TOKEN, "auth": "cli"}));
    }

    #[test]
    fn login_mismatch_is_a_uniform_401() {
        for body in [
            r#"{"username":"alice","password":"wrong-password-wrong"}"#,
            r#"{"username":"bob","password":"correct-horse-battery-staple"}"#,
            r#"{"username":"ALICE","password":"correct-horse-battery-staple"}"#,
            r#"{"username":"","password":""}"#,
        ] {
            let resp = creds().login(body.as_bytes());
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(reason_of(&resp), "password is not correct");
        }
    }

    #[test]
    fn malformed_login_bodies() {
        for body in [
            &b""[..],
            b"not json",
            b"[]",
            br#"{"username":"alice"}"#,
            br#"{"password":"correct-horse-battery-staple"}"#,
            br#"{"username":1,"password":"x"}"#,
        ] {
            let resp = creds().login(body);
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_eq!(reason_of(&resp), "invalid request body");
        }
    }
}
