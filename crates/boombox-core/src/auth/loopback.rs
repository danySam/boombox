use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};

use percent_encoding::percent_decode_str;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{Error, Result};

/// Spotify rejects `localhost` as a redirect host and only permits plain HTTP
/// for loopback IP literals, so this is the one address that works.
pub const REDIRECT_HOST: &str = "127.0.0.1";
pub const CALLBACK_PATH: &str = "/callback";

/// Why the redirect listener could not start, in terms someone can act on.
///
/// A taken port is the likely first-run failure -- 8888 is also Jupyter's
/// default -- and a bare "Address already in use" says nothing about the
/// Redirect URI registered with Spotify, which has to change with the port.
fn bind_failure(port: u16, error: &std::io::Error) -> String {
    if error.kind() != std::io::ErrorKind::AddrInUse {
        return format!("cannot bind {REDIRECT_HOST}:{port} for the OAuth redirect: {error}");
    }
    let example = if port == 8888 { " (Jupyter uses 8888 too)" } else { "" };
    format!(
        "port {port} is already in use by another program{example}, and sign-in needs it to \
         receive Spotify's answer. Close that program and try again, or pick another port: \
         set redirect_port in the config, and change the Redirect URI in your Spotify app's \
         settings to match."
    )
}

pub struct Loopback {
    listener: TcpListener,
    addr: SocketAddr,
}

impl Loopback {
    /// Port 0 asks the OS for an ephemeral one. Spotify allows loopback
    /// redirects to use a port that wasn't registered in advance, so this
    /// still matches a dashboard entry of `http://127.0.0.1:8888/callback`
    /// only if the ports agree -- prefer an explicit port unless you know
    /// your app registration is port-agnostic.
    pub async fn bind(port: u16) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|e| Error::Authorization(bind_failure(port, &e)))?;
        let addr = listener.local_addr()?;
        Ok(Self { listener, addr })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://{REDIRECT_HOST}:{}{CALLBACK_PATH}", self.port())
    }

    /// Serves until a request hits the callback path. Browsers cheerfully
    /// request `/favicon.ico` and speculatively preconnect, so anything that
    /// isn't the callback gets a 404 and we keep waiting.
    pub async fn wait_for_callback(&self) -> Result<HashMap<String, String>> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let Some(target) = read_request_target(&mut stream).await? else {
                continue;
            };

            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p, q),
                None => (target.as_str(), ""),
            };

            if path != CALLBACK_PATH {
                respond(&mut stream, "404 Not Found", NOT_FOUND_PAGE).await?;
                continue;
            }

            let params = parse_query(query);

            if let Some(err) = params.get("error") {
                respond(&mut stream, "400 Bad Request", &result_page(Some(err))).await?;
                return Err(Error::Authorization(match err.as_str() {
                    "access_denied" => "you declined the authorization request".into(),
                    other => other.to_string(),
                }));
            }

            respond(&mut stream, "200 OK", &result_page(None)).await?;
            return Ok(params);
        }
    }
}

/// Reads just enough to get the request line. We never need the body.
async fn read_request_target(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];

    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);

        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
            return Ok(line.split_whitespace().nth(1).map(str::to_owned));
        }
        // A request line this long is not a browser talking to us.
        if buf.len() > 8192 {
            break;
        }
    }
    Ok(None)
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((decode(k), decode(v)))
        })
        .collect()
}

fn decode(s: &str) -> String {
    // Query strings encode spaces as '+', which percent-decoding leaves alone.
    let plus_decoded = s.replace('+', " ");
    percent_decode_str(&plus_decoded).decode_utf8_lossy().into_owned()
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

const NOT_FOUND_PAGE: &str = "<!doctype html><title>boombox</title><p>Not here.</p>";

fn result_page(error: Option<&str>) -> String {
    let (heading, detail) = match error {
        None => ("Authorized", "You can close this tab and return to the terminal."),
        Some("access_denied") => ("Declined", "You declined the request. Nothing was saved."),
        Some(_) => ("Authorization failed", "Check the terminal for details."),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>boombox</title>\
         <style>\
         :root{{color-scheme:light dark}}\
         body{{font:16px/1.5 ui-sans-serif,system-ui,sans-serif;display:grid;\
         place-items:center;min-height:100vh;margin:0;text-align:center}}\
         h1{{font-size:1.5rem;margin:0 0 .5rem;letter-spacing:-.02em}}\
         p{{margin:0;opacity:.65}}\
         </style></head><body><div><h1>{heading}</h1><p>{detail}</p></div></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw error names neither the other program nor the Redirect URI
    /// that has to move with the port.
    #[test]
    fn a_taken_port_says_what_to_do_about_it() {
        let taken = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let message = bind_failure(8888, &taken);
        assert!(message.contains("already in use") && message.contains("Jupyter"), "{message}");
        assert!(message.contains("redirect_port") && message.contains("Redirect URI"), "{message}");
        assert!(!bind_failure(9000, &taken).contains("Jupyter"), "the example only fits 8888");

        let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(bind_failure(80, &other).starts_with("cannot bind 127.0.0.1:80"));
    }

    #[test]
    fn parses_code_and_state() {
        let p = parse_query("code=AQD123&state=xyz");
        assert_eq!(p["code"], "AQD123");
        assert_eq!(p["state"], "xyz");
    }

    #[test]
    fn percent_and_plus_are_decoded() {
        let p = parse_query("error_description=User+said+no%21&error=access_denied");
        assert_eq!(p["error_description"], "User said no!");
        assert_eq!(p["error"], "access_denied");
    }

    #[test]
    fn empty_query_is_empty_map() {
        assert!(parse_query("").is_empty());
    }

    #[tokio::test]
    async fn ephemeral_port_produces_a_loopback_redirect_uri() {
        let lo = Loopback::bind(0).await.unwrap();
        assert!(lo.port() > 0);
        let uri = lo.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/callback"), "{uri}");
    }
}
