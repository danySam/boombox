pub mod library;
pub mod library_models;
pub mod models;
pub mod player;
pub mod player_models;

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::auth::Auth;
use crate::error::{Error, Result};
pub use library::LibraryApi;
pub use library_models::{
    Entry, EntryKind, Page, PlaylistItem, SavedAlbum, SavedTrack, SearchResults, SearchType,
    SimplePlaylist,
};
pub use models::CurrentUser;
pub use player::{Offset, PlayOptions, Playback, PlayerApi, SpectrumApi};
pub use player_models::{Device, Devices, PlaybackState, PlayingItem, Queue, RepeatState, Track};

pub const API_BASE: &str = "https://api.spotify.com/v1";

pub struct Client {
    auth: Arc<Auth>,
    http: reqwest::Client,
}

/// A request under construction. Exists so the player methods can express
/// query parameters and bodies without a bespoke function per endpoint.
pub struct Request {
    method: reqwest::Method,
    path: String,
    query: Vec<(String, String)>,
    body: Option<serde_json::Value>,
}

impl Request {
    pub fn new(method: reqwest::Method, path: impl Into<String>) -> Self {
        Self { method, path: path.into(), query: Vec::new(), body: None }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self::new(reqwest::Method::GET, path)
    }

    pub fn put(path: impl Into<String>) -> Self {
        Self::new(reqwest::Method::PUT, path)
    }

    pub fn post(path: impl Into<String>) -> Self {
        Self::new(reqwest::Method::POST, path)
    }

    pub fn query(mut self, key: &str, value: impl ToString) -> Self {
        self.query.push((key.to_string(), value.to_string()));
        self
    }

    pub fn maybe_query(self, key: &str, value: Option<impl ToString>) -> Self {
        match value {
            Some(v) => self.query(key, v),
            None => self,
        }
    }

    pub fn json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

impl Client {
    pub fn new(auth: Arc<Auth>) -> Self {
        Self {
            auth,
            http: reqwest::Client::builder()
                .user_agent(concat!("boombox/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("rustls client builds"),
        }
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Raw request escape hatch: returns the HTTP status and body without
    /// interpreting either. Spotify reshapes this API often enough that being
    /// able to poke it with your own session is worth a public method.
    pub async fn raw(&self, method: &str, path: &str) -> Result<(u16, String)> {
        let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
            .map_err(|e| Error::Config(format!("bad method: {e}")))?;
        let token = self.auth.access_token().await?;
        let resp = self
            .http
            .request(method, format!("{API_BASE}{path}"))
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .send()
            .await?;
        let status = resp.status().as_u16();
        Ok((status, resp.text().await?))
    }

    pub async fn current_user(&self) -> Result<CurrentUser> {
        self.json(Request::get("/me")).await
    }

    /// Sends a request and decodes the body. A 204 is an error here; use
    /// [`Client::json_opt`] for endpoints that legitimately return no content.
    pub async fn json<T: DeserializeOwned>(&self, req: Request) -> Result<T> {
        self.json_opt(req).await?.ok_or_else(|| Error::Api {
            status: 204,
            message: "expected a response body but got none".into(),
        })
    }

    pub async fn json_opt<T: DeserializeOwned>(&self, req: Request) -> Result<Option<T>> {
        match self.send(req).await? {
            Some(body) => Ok(Some(serde_json::from_str(&body)?)),
            None => Ok(None),
        }
    }

    /// For the many player endpoints that answer 204 with an empty body.
    pub async fn empty(&self, req: Request) -> Result<()> {
        self.send(req).await.map(|_| ())
    }

    /// Returns `None` for 204 / an empty body.
    async fn send(&self, req: Request) -> Result<Option<String>> {
        let url = format!("{API_BASE}{}", req.path);
        let token = self.auth.access_token().await?;

        let mut builder = self.http.request(req.method, &url).bearer_auth(&token);
        if !req.query.is_empty() {
            builder = builder.query(&req.query);
        }
        // Spotify rejects some PUTs that arrive with no Content-Length, so
        // always send an explicit body even when it is empty.
        builder = match req.body {
            Some(body) => builder.json(&body),
            None => builder.header(reqwest::header::CONTENT_LENGTH, "0"),
        };

        let resp = builder.send().await?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let body = resp.text().await?;

        if status.is_success() {
            return Ok(if body.trim().is_empty() { None } else { Some(body) });
        }
        Err(classify(status.as_u16(), &body, retry_after))
    }
}

/// Spotify overloads a handful of status codes; turn them into errors that say
/// what the user should actually do.
fn classify(status: u16, body: &str, retry_after: Option<u64>) -> Error {
    let detail = ErrorDetail::parse(body);
    let message = detail.message.clone().unwrap_or_else(|| body.trim().to_string());
    let reason = detail.reason.unwrap_or_default();
    let lower = message.to_ascii_lowercase();

    match status {
        401 => Error::NotAuthenticated,
        403 if reason == "PREMIUM_REQUIRED" || lower.contains("premium") => Error::PremiumRequired,
        404 if reason == "NO_ACTIVE_DEVICE" || lower.contains("device") => Error::NoActiveDevice,
        429 => Error::Api {
            status,
            message: match retry_after {
                Some(s) => format!("rate limited, retry in {s}s"),
                None => "rate limited".into(),
            },
        },
        _ => Error::Api { status, message },
    }
}

#[derive(Default)]
struct ErrorDetail {
    message: Option<String>,
    reason: Option<String>,
}

impl ErrorDetail {
    fn parse(body: &str) -> Self {
        #[derive(serde::Deserialize)]
        struct Envelope {
            error: Inner,
        }
        #[derive(serde::Deserialize)]
        struct Inner {
            message: Option<String>,
            reason: Option<String>,
        }
        serde_json::from_str::<Envelope>(body)
            .map(|e| Self { message: e.error.message, reason: e.error.reason })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthorized_maps_to_not_authenticated() {
        let e =
            classify(401, r#"{"error":{"status":401,"message":"The access token expired"}}"#, None);
        assert!(matches!(e, Error::NotAuthenticated));
    }

    #[test]
    fn premium_gate_is_recognised_by_message_or_reason() {
        let by_message = classify(
            403,
            r#"{"error":{"status":403,"message":"Player command failed: Premium required"}}"#,
            None,
        );
        assert!(matches!(by_message, Error::PremiumRequired));

        let by_reason = classify(
            403,
            r#"{"error":{"status":403,"message":"Forbidden","reason":"PREMIUM_REQUIRED"}}"#,
            None,
        );
        assert!(matches!(by_reason, Error::PremiumRequired));
    }

    #[test]
    fn missing_device_is_recognised_by_message_or_reason() {
        let by_message = classify(
            404,
            r#"{"error":{"status":404,"message":"Player command failed: No active device found"}}"#,
            None,
        );
        assert!(matches!(by_message, Error::NoActiveDevice));

        let by_reason = classify(
            404,
            r#"{"error":{"status":404,"message":"Not found","reason":"NO_ACTIVE_DEVICE"}}"#,
            None,
        );
        assert!(matches!(by_reason, Error::NoActiveDevice));
    }

    #[test]
    fn a_genuine_404_is_not_mistaken_for_a_missing_device() {
        let e = classify(404, r#"{"error":{"status":404,"message":"Non existing id"}}"#, None);
        assert!(matches!(e, Error::Api { status: 404, .. }), "{e}");
    }

    #[test]
    fn rate_limit_surfaces_retry_after() {
        let e = classify(429, "{}", Some(12));
        assert!(e.to_string().contains("retry in 12s"), "{e}");
    }

    #[test]
    fn other_errors_keep_the_spotify_message() {
        let e = classify(400, r#"{"error":{"status":400,"message":"Invalid track uri"}}"#, None);
        assert!(e.to_string().contains("Invalid track uri"), "{e}");
    }

    #[test]
    fn non_json_bodies_do_not_panic() {
        let e = classify(502, "<html>Bad Gateway</html>", None);
        assert!(e.to_string().contains("Bad Gateway"), "{e}");
    }

    #[test]
    fn request_builder_skips_absent_optional_query_params() {
        let r = Request::put("/me/player/play")
            .maybe_query("device_id", None::<String>)
            .query("position_ms", 1500);
        assert_eq!(r.query, vec![("position_ms".to_string(), "1500".to_string())]);
    }
}
