pub mod loopback;
pub mod pkce;
pub mod token;

use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
pub use loopback::Loopback;
pub use pkce::Pkce;
pub use token::{StorageBackend, TokenResponse, TokenStore, Tokens};

pub const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
pub const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

/// Everything boombox needs across all phases, requested once so users approve a
/// single consent screen. Endpoints that these used to unlock and no longer
/// exist (browse, recommendations, audio features) are deliberately absent.
pub const SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-recently-played",
    "user-read-private",
    "user-library-read",
    "user-library-modify",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-follow-read",
    "user-follow-modify",
    // /me/top/{artists,tracks} still exists and 403s without this. With
    // recommendations and browse gone it is one of the few discovery
    // surfaces left, so ask now rather than force a second consent later.
    "user-top-read",
    // Only meaningful once the librespot feature lands, but asking now saves
    // a second consent round-trip later.
    "streaming",
];

/// `application/x-www-form-urlencoded` leaves these alone; everything else in
/// a query value gets escaped.
const QUERY_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

fn esc(s: &str) -> String {
    utf8_percent_encode(s, QUERY_SET).to_string()
}

pub struct Auth {
    client_id: String,
    http: reqwest::Client,
    tokens: Mutex<Option<Tokens>>,
    store: TokenStore,
}

impl Auth {
    pub fn new(client_id: impl Into<String>, tokens: Option<Tokens>) -> Self {
        Self::with_store(client_id, tokens, TokenStore::default())
    }

    pub fn with_store(
        client_id: impl Into<String>,
        tokens: Option<Tokens>,
        store: TokenStore,
    ) -> Self {
        Self {
            store,
            client_id: client_id.into(),
            http: reqwest::Client::builder()
                .user_agent(concat!("boombox/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("rustls client builds"),
            tokens: Mutex::new(tokens),
        }
    }

    /// Reads the client ID from config and any stored tokens from the token store.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let client_id = config.resolve_client_id()?;
        let store = TokenStore::from_config(config);
        Ok(Self::with_store(client_id, store.load()?, store))
    }

    pub fn store(&self) -> TokenStore {
        self.store
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub async fn tokens(&self) -> Option<Tokens> {
        self.tokens.lock().await.clone()
    }

    /// A token guaranteed valid for at least the refresh skew, refreshing
    /// transparently if needed. This is the only method callers should use.
    pub async fn access_token(&self) -> Result<String> {
        let mut guard = self.tokens.lock().await;
        let current = guard.as_ref().ok_or(Error::NotAuthenticated)?;

        if !current.is_expired() {
            return Ok(current.access_token.clone());
        }

        tracing::debug!("access token expired, refreshing");
        let refreshed = self.refresh_locked(&mut guard).await?;
        Ok(refreshed.access_token)
    }

    /// Exchanges the refresh token regardless of expiry. Mostly useful for
    /// proving the refresh path works before it is needed in anger.
    pub async fn refresh(&self) -> Result<Tokens> {
        let mut guard = self.tokens.lock().await;
        self.refresh_locked(&mut guard).await
    }

    async fn refresh_locked(&self, guard: &mut Option<Tokens>) -> Result<Tokens> {
        let current = guard.as_ref().ok_or(Error::NotAuthenticated)?;
        let previous_refresh = current.refresh_token.clone();

        let refreshed = self
            .request_tokens(
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &previous_refresh),
                    ("client_id", &self.client_id),
                ],
                Some(&previous_refresh),
            )
            .await?;

        self.store.save(&refreshed)?;
        *guard = Some(refreshed.clone());
        Ok(refreshed)
    }

    /// Runs the full authorization-code-with-PKCE flow. `present` receives the
    /// authorize URL so the caller can open a browser and/or print it.
    pub async fn login<F>(
        &self,
        redirect_port: u16,
        timeout: Duration,
        present: F,
    ) -> Result<Tokens>
    where
        F: FnOnce(&str),
    {
        let server = Loopback::bind(redirect_port).await?;
        let redirect_uri = server.redirect_uri();
        let pkce = Pkce::generate();
        let state = pkce::random_string(32);

        present(&authorize_url(&self.client_id, &redirect_uri, &pkce.challenge, &state));

        let params = tokio::time::timeout(timeout, server.wait_for_callback())
            .await
            .map_err(|_| Error::AuthorizationTimeout(timeout.as_secs()))??;

        // Guards against a third party feeding us a code from another session.
        let returned_state = params.get("state").map(String::as_str).unwrap_or_default();
        if returned_state != state {
            return Err(Error::Authorization(
                "state mismatch -- the callback did not come from this login attempt".into(),
            ));
        }

        let code = params
            .get("code")
            .ok_or_else(|| Error::Authorization("callback carried no authorization code".into()))?;

        let tokens = self
            .request_tokens(
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &redirect_uri),
                    ("client_id", &self.client_id),
                    ("code_verifier", &pkce.verifier),
                ],
                None,
            )
            .await?;

        self.store.save(&tokens)?;
        *self.tokens.lock().await = Some(tokens.clone());
        Ok(tokens)
    }

    async fn request_tokens(
        &self,
        form: &[(&str, &str)],
        previous_refresh: Option<&str>,
    ) -> Result<Tokens> {
        let resp = self.http.post(TOKEN_URL).form(form).send().await?;
        let status = resp.status();
        let body = resp.text().await?;

        if !status.is_success() {
            return Err(Error::Authorization(describe_token_error(status, &body)));
        }

        let parsed: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| Error::Authorization(format!("could not parse token response: {e}")))?;
        Tokens::from_response(parsed, previous_refresh)
    }
}

pub fn authorize_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&response_type=code&redirect_uri={}\
         &code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        esc(client_id),
        esc(redirect_uri),
        esc(challenge),
        esc(state),
        esc(&SCOPES.join(" ")),
    )
}

/// What Spotify says about a client ID, asked without anyone signing in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCheck {
    /// Spotify has an app with this ID.
    Known,
    /// Spotify has no app with this ID -- often the Client Secret, pasted in
    /// its place, since the two look alike.
    Unknown,
    /// No answer that settles it: offline, rate limited, or a reply this does
    /// not recognise. Never a reason to stop.
    Unchecked,
}

/// Asks Spotify whether `client_id` belongs to an app.
///
/// There is no endpoint for this, so it spends a code exchange that cannot
/// succeed. The token endpoint looks up the client before the code: an app
/// it has never heard of gets `invalid_client`, a real app with a useless code
/// gets `invalid_grant` -- the two errors OAuth defines for exactly that
/// difference (RFC 6749, section 5.2). Nothing is authorised or stored.
///
/// It says nothing about the redirect URI, which Spotify checks only once
/// someone has signed in.
pub async fn check_client_id(client_id: &str) -> ClientCheck {
    check_client_id_at(TOKEN_URL, client_id).await
}

/// Long enough for a slow connection, short enough that setup does not seem
/// to hang when there is none.
const CLIENT_CHECK_TIMEOUT: Duration = Duration::from_secs(8);

async fn check_client_id_at(url: &str, client_id: &str) -> ClientCheck {
    let Ok(http) = reqwest::Client::builder()
        .user_agent(concat!("boombox/", env!("CARGO_PKG_VERSION")))
        .timeout(CLIENT_CHECK_TIMEOUT)
        .build()
    else {
        return ClientCheck::Unchecked;
    };
    // The verifier is the RFC 7636 example: any well-formed one will do, since
    // the exchange fails before it is compared.
    let form = [
        ("grant_type", "authorization_code"),
        ("code", "boombox-client-id-check"),
        ("redirect_uri", "http://127.0.0.1/callback"),
        ("client_id", client_id),
        ("code_verifier", "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
    ];
    let response = match http.post(url).form(&form).send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::debug!("could not check the client ID: {e}");
            return ClientCheck::Unchecked;
        }
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    classify_client_check(status, &body)
}

/// Only the two OAuth errors settle it; anything else is not a verdict.
fn classify_client_check(status: reqwest::StatusCode, body: &str) -> ClientCheck {
    #[derive(serde::Deserialize)]
    struct OAuthError {
        error: String,
    }

    if status != reqwest::StatusCode::BAD_REQUEST && status != reqwest::StatusCode::UNAUTHORIZED {
        return ClientCheck::Unchecked;
    }
    match serde_json::from_str::<OAuthError>(body).map(|e| e.error) {
        Ok(error) if error == "invalid_client" => ClientCheck::Unknown,
        Ok(error) if error == "invalid_grant" => ClientCheck::Known,
        _ => ClientCheck::Unchecked,
    }
}

fn describe_token_error(status: reqwest::StatusCode, body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct OAuthError {
        error: String,
        error_description: Option<String>,
    }

    let Ok(parsed) = serde_json::from_str::<OAuthError>(body) else {
        return format!("token endpoint returned {status}: {body}");
    };

    let detail = parsed.error_description.unwrap_or_default();
    match parsed.error.as_str() {
        "invalid_client" => {
            "invalid client_id. Check it against your app in the Spotify dashboard.".into()
        }
        "invalid_grant" if detail.contains("redirect") => format!(
            "{detail}. The redirect URI must match your dashboard entry exactly, \
             including the port and the /callback path -- and must use 127.0.0.1, \
             not localhost."
        ),
        "invalid_grant" => format!("{detail}. Try `boombox auth login` again."),
        _ if detail.is_empty() => format!("{} ({status})", parsed.error),
        _ => format!("{}: {detail}", parsed.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_every_required_param() {
        let url = authorize_url("cid123", "http://127.0.0.1:8888/callback", "chal", "st");
        assert!(url.starts_with(AUTHORIZE_URL));
        for expected in [
            "client_id=cid123",
            "response_type=code",
            "code_challenge_method=S256",
            "code_challenge=chal",
            "state=st",
        ] {
            assert!(url.contains(expected), "missing {expected} in {url}");
        }
    }

    #[test]
    fn redirect_uri_is_escaped() {
        let url = authorize_url("c", "http://127.0.0.1:8888/callback", "x", "y");
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8888%2Fcallback"), "{url}");
        assert!(!url.contains("redirect_uri=http://"), "{url}");
    }

    #[test]
    fn scopes_are_space_delimited_and_escaped() {
        let url = authorize_url("c", "r", "x", "y");
        assert!(url.contains("user-read-playback-state%20user-modify-playback-state"), "{url}");
    }

    #[test]
    fn scope_list_has_no_duplicates() {
        let mut sorted = SCOPES.to_vec();
        sorted.sort_unstable();
        let len = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len);
    }

    #[test]
    fn token_errors_are_translated_to_something_actionable() {
        let msg = describe_token_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_client","error_description":"Invalid client"}"#,
        );
        assert!(msg.contains("dashboard"), "{msg}");

        let msg = describe_token_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant","error_description":"Invalid redirect URI"}"#,
        );
        assert!(msg.contains("127.0.0.1"), "{msg}");
    }

    #[test]
    fn unparseable_error_bodies_still_surface_the_status() {
        let msg = describe_token_error(reqwest::StatusCode::BAD_GATEWAY, "<html>nope</html>");
        assert!(msg.contains("502"), "{msg}");
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    /// Both bodies are what Spotify actually sent, for a made-up ID and for a
    /// real app's.
    #[test]
    fn only_the_two_oauth_errors_settle_a_client_check() {
        use reqwest::StatusCode as S;
        let unknown = r#"{"error":"invalid_client","error_description":"Failed to get client"}"#;
        let known = r#"{"error":"invalid_grant","error_description":"Invalid authorization code"}"#;
        assert_eq!(classify_client_check(S::BAD_REQUEST, unknown), ClientCheck::Unknown);
        assert_eq!(classify_client_check(S::BAD_REQUEST, known), ClientCheck::Known);
        assert_eq!(classify_client_check(S::UNAUTHORIZED, unknown), ClientCheck::Unknown);

        let other = r#"{"error":"invalid_request"}"#;
        assert_eq!(classify_client_check(S::BAD_REQUEST, other), ClientCheck::Unchecked);
        assert_eq!(classify_client_check(S::BAD_REQUEST, "<html>"), ClientCheck::Unchecked);
        assert_eq!(classify_client_check(S::TOO_MANY_REQUESTS, ""), ClientCheck::Unchecked);
        assert_eq!(
            classify_client_check(S::SERVICE_UNAVAILABLE, unknown),
            ClientCheck::Unchecked,
            "an outage is not a verdict, whatever the body says"
        );
        assert_eq!(classify_client_check(S::OK, "{}"), ClientCheck::Unchecked);
    }

    /// Serves one canned response and hands back the request it answered.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            // Until the whole form has arrived, going by Content-Length.
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head) = text.find("\r\n\r\n") {
                    let length = text[..head]
                        .lines()
                        .find_map(|line| {
                            let line = line.to_ascii_lowercase();
                            line.strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if request.len() >= head + 4 + length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, server)
    }

    #[tokio::test]
    async fn an_app_spotify_has_never_heard_of_is_unknown() {
        let body = r#"{"error":"invalid_client","error_description":"Failed to get client"}"#;
        let (url, server) = serve_once("400 Bad Request", body).await;
        assert_eq!(check_client_id_at(&url, ID).await, ClientCheck::Unknown);

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /api/token"), "{request}");
        assert!(request.contains(&format!("client_id={ID}")), "{request}");
        assert!(request.contains("grant_type=authorization_code"), "{request}");
    }

    #[tokio::test]
    async fn a_real_app_with_a_useless_code_is_known() {
        let body = r#"{"error":"invalid_grant","error_description":"Invalid authorization code"}"#;
        let (url, server) = serve_once("400 Bad Request", body).await;
        assert_eq!(check_client_id_at(&url, ID).await, ClientCheck::Known);
        server.await.unwrap();
    }

    /// Offline must never read as a wrong ID, or setup would refuse good ones.
    #[tokio::test]
    async fn no_answer_at_all_is_unchecked() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/api/token");
        assert_eq!(check_client_id_at(&url, ID).await, ClientCheck::Unchecked);
    }
}
