//! MCP remote OAuth 2.1 — authorization code + PKCE(S256) + two-step discovery
//! (plan 34b). This is the wire protocol only: PKCE, RFC 9728/8414 discovery,
//! RFC 7591 dynamic client registration, the token exchange/refresh POSTs, a
//! loopback callback listener, and the request-time [`OAuthSession`] that
//! injects a bearer and refreshes it. Config, on-disk token storage, and
//! opening the browser live in the CLI (this crate never touches the terminal
//! or a config file) — the same split as the rest of kloop-mcp.
//!
//! Both reference implementations lean on a heavy OAuth crate (codex's `rmcp`
//! → `oauth2`; cc's SDK `auth.js`); kloop hand-writes the wire (a few GETs, two
//! form POSTs, a PKCE hash) and borrows only `sha2`/`getrandom`, matching the
//! "small crate, not a framework" line the SSE parser and HTML→text follow.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use url::Url;

/// Refresh a token this many seconds before its absolute expiry so an in-flight
/// request doesn't race the deadline. cc uses 300s, codex 30s; the 401
/// fallback ([`OAuthSession::refresh_after_401`]) catches whatever still slips.
const REFRESH_SKEW_SECS: u64 = 60;

/// How long the loopback listener waits for the browser redirect before giving
/// up (both references use 5 minutes).
pub(crate) const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// The loopback redirect path. RFC 8252 §7.3: for a loopback redirect only the
/// path must match a registration — the port is chosen fresh each login.
const REDIRECT_PATH: &str = "/callback";

/// A bearer credential from a token endpoint. Serialized into the CLI's on-disk
/// store; `expires_at` is ABSOLUTE (unix seconds) so a later process can judge
/// staleness — storing a relative `expires_in` can't survive a restart (both
/// references learned this and store absolute).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// What a successful [`login`] yields — the token plus the facts the CLI must
/// persist to make request-time refresh self-contained (no re-discovery on
/// every start): the token endpoint, the resolved client_id, and the RFC 8707
/// `resource` (audience) that authorize/token/refresh all bind to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginOutcome {
    pub token: OAuthToken,
    pub client_id: String,
    pub token_endpoint: String,
    pub resource: String,
}

/// Endpoints resolved from RFC 8414 authorization-server metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Option<Vec<String>>,
}

/// The token-endpoint JSON, shared by the code exchange and the refresh.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

impl TokenResponse {
    /// Fold into an [`OAuthToken`], turning `expires_in` into an absolute
    /// deadline and keeping `prev_refresh` when the server didn't rotate the
    /// refresh token (refresh responses often omit it).
    fn into_token(self, prev_refresh: Option<String>, now: u64) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token.or(prev_refresh),
            expires_at: self.expires_in.map(|s| now.saturating_add(s)),
            scope: self.scope,
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when `token` expires within the proactive skew (so it should be
/// refreshed before use). A token with no `expires_at` never triggers this —
/// its only staleness signal is a 401.
fn near_expiry(token: &OAuthToken, now: u64) -> bool {
    token
        .expires_at
        .is_some_and(|exp| exp <= now.saturating_add(REFRESH_SKEW_SECS))
}

// ---------------------------------------------------------------------------
// PKCE + CSRF state
// ---------------------------------------------------------------------------

struct Pkce {
    verifier: String,
    challenge: String,
}

/// PKCE S256: a random 32-byte verifier (base64url), challenge =
/// base64url(sha256(verifier)). Only held for the single flow — never stored.
fn generate_pkce() -> Result<Pkce> {
    let verifier = random_token()?;
    let digest = Sha256::digest(verifier.as_bytes());
    Ok(Pkce {
        challenge: URL_SAFE_NO_PAD.encode(digest),
        verifier,
    })
}

/// A URL-safe random token from 32 CSPRNG bytes — the PKCE verifier and the
/// CSRF `state` are both this.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

// ---------------------------------------------------------------------------
// Discovery (RFC 9728 protected-resource → RFC 8414 authorization-server)
// ---------------------------------------------------------------------------

/// Step 1: find the protected-resource metadata (PRM) URL. Preferred source is
/// the `WWW-Authenticate: … resource_metadata="…"` on an unauthenticated 401
/// (RFC 9728); absent that, the well-known default at the server's origin.
async fn discover_prm_url(http: &reqwest::Client, server_url: &str) -> Result<String> {
    let resp = http
        .get(server_url)
        .header(ACCEPT, "application/json")
        .send()
        .await;
    if let Ok(resp) = resp {
        if let Some(header) = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(url) = parse_resource_metadata(header) {
                return Ok(url);
            }
        }
    }
    let origin = origin_of(server_url)?;
    Ok(format!("{origin}/.well-known/oauth-protected-resource"))
}

/// Pull `resource_metadata="<url>"` out of a `WWW-Authenticate` header value.
/// The value may be quoted or a bare token; other auth-params are ignored.
fn parse_resource_metadata(header: &str) -> Option<String> {
    let idx = header.find("resource_metadata")?;
    let rest = header[idx + "resource_metadata".len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([' ', ',']).next()?,
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// Step 2: GET the PRM document and take its first authorization server, plus
/// any `scopes_supported` it advertises.
async fn fetch_prm(http: &reqwest::Client, prm_url: &str) -> Result<(String, Option<Vec<String>>)> {
    let doc: Value = http
        .get(prm_url)
        .header(ACCEPT, "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("protected-resource metadata is not JSON")?;
    let issuer = doc["authorization_servers"][0]
        .as_str()
        .context("protected-resource metadata has no authorization_servers")?
        .to_string();
    Ok((issuer, string_array(&doc["scopes_supported"])))
}

/// Step 3: fetch the authorization-server metadata for `issuer`. Tries the
/// path-aware RFC 8414 variants first (path-scoped issuers are common), falling
/// back to OIDC discovery, until one returns usable endpoints.
async fn fetch_as_metadata(http: &reqwest::Client, issuer: &str) -> Result<AuthServerMetadata> {
    let mut last_err = None;
    for candidate in as_metadata_candidates(issuer)? {
        match http
            .get(&candidate)
            .header(ACCEPT, "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                Ok(doc) => {
                    if let Some(meta) = parse_as_metadata(&doc) {
                        return Ok(meta);
                    }
                    last_err = Some(anyhow!("{candidate}: metadata missing endpoints"));
                }
                Err(e) => last_err = Some(anyhow!("{candidate}: invalid JSON: {e}")),
            },
            Ok(resp) => last_err = Some(anyhow!("{candidate}: {}", resp.status())),
            Err(e) => last_err = Some(anyhow!("{candidate}: {e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no authorization-server metadata for {issuer}")))
}

fn parse_as_metadata(doc: &Value) -> Option<AuthServerMetadata> {
    let authorization_endpoint = doc["authorization_endpoint"].as_str()?.to_string();
    let token_endpoint = doc["token_endpoint"].as_str()?.to_string();
    Some(AuthServerMetadata {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: doc["registration_endpoint"].as_str().map(str::to_string),
        scopes_supported: string_array(&doc["scopes_supported"]),
    })
}

/// The RFC 8414 / OIDC well-known URLs to probe for `issuer`. For a path-scoped
/// issuer (`https://h/tenant`) that means the path-aware form
/// (`https://h/.well-known/oauth-authorization-server/tenant`) AND the legacy
/// nested form (`https://h/tenant/.well-known/…`); for a bare origin just the
/// two root variants.
fn as_metadata_candidates(issuer: &str) -> Result<Vec<String>> {
    let url = Url::parse(issuer).with_context(|| format!("invalid issuer url '{issuer}'"))?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let well_known = ["oauth-authorization-server", "openid-configuration"];
    let mut out = Vec::new();
    if path.is_empty() {
        for wk in well_known {
            out.push(format!("{origin}/.well-known/{wk}"));
        }
    } else {
        for wk in well_known {
            out.push(format!("{origin}/.well-known/{wk}{path}"));
            out.push(format!("{origin}{path}/.well-known/{wk}"));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

/// Register a public client (`token_endpoint_auth_method: "none"`) and return
/// its `client_id`. Used only when no client_id was preconfigured.
async fn register_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<String> {
    let body = json!({
        "client_name": "kloop",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let doc: Value = http
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("client registration response is not JSON")?;
    doc["client_id"]
        .as_str()
        .map(str::to_string)
        .context("client registration response has no client_id")
}

// ---------------------------------------------------------------------------
// Authorize URL + loopback callback
// ---------------------------------------------------------------------------

/// Build the authorization-request URL: response_type=code, PKCE S256, the CSRF
/// `state`, and (RFC 8707) `resource` binding the grant to the MCP server.
fn build_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scope: Option<&str>,
    resource: &str,
) -> Result<String> {
    let mut url = Url::parse(authorization_endpoint)
        .with_context(|| format!("invalid authorization_endpoint '{authorization_endpoint}'"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("code_challenge", challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", state);
        if let Some(scope) = scope.filter(|s| !s.is_empty()) {
            q.append_pair("scope", scope);
        }
        q.append_pair("resource", resource);
    }
    Ok(url.into())
}

/// Wait for the browser to hit the loopback redirect, returning the `code` once
/// the `state` matches. Rejects a mismatched state (CSRF) and an `error=`
/// redirect; any other path (favicon, root) is answered and ignored. Bounded by
/// `timeout`.
async fn await_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let accept = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let line = read_request_line(&mut stream).await?;
            let target = line.split_whitespace().nth(1).unwrap_or("");
            let params = parse_query(target);
            if let Some(error) = params.get("error") {
                let desc = params.get("error_description").map(String::as_str);
                write_response(
                    &mut stream,
                    "Authorization failed",
                    "You can close this window.",
                )
                .await;
                bail!(
                    "authorization server returned error '{error}'{}",
                    desc.map(|d| format!(": {d}")).unwrap_or_default()
                );
            }
            let (Some(code), Some(state)) = (params.get("code"), params.get("state")) else {
                write_response(&mut stream, "Waiting…", "Waiting for the OAuth redirect.").await;
                continue;
            };
            if state != expected_state {
                write_response(&mut stream, "State mismatch", "Login aborted.").await;
                bail!("OAuth state mismatch (possible CSRF); login aborted");
            }
            write_response(
                &mut stream,
                "Login complete",
                "kloop is now authorized. You can close this window.",
            )
            .await;
            return Ok(code.clone());
        }
    };
    match tokio::time::timeout(timeout, accept).await {
        Ok(result) => result,
        Err(_) => bail!("timed out after {timeout:?} waiting for the OAuth callback"),
    }
}

/// Read the HTTP request line (up to the first newline) — all we need is the
/// `GET <target> HTTP/1.1`. Bounded so a misbehaving client can't stream
/// forever.
async fn read_request_line(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte).await? == 1 {
        match byte[0] {
            b'\n' => break,
            b'\r' => {}
            b => buf.push(b),
        }
        if buf.len() > 8192 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parse a request target's query string into a map, percent-decoding values.
fn parse_query(target: &str) -> HashMap<String, String> {
    match Url::parse(&format!("http://127.0.0.1{target}")) {
        Ok(url) => url.query_pairs().into_owned().collect(),
        Err(_) => HashMap::new(),
    }
}

async fn write_response(stream: &mut tokio::net::TcpStream, title: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:32rem;margin:4rem auto\">\
         <h2>{title}</h2><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

/// The client-identity fields the authorization-code exchange sends alongside
/// the per-flow `code`/`verifier` (grouped so the exchange stays a few args).
struct ExchangeParams<'a> {
    token_endpoint: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    resource: &'a str,
}

/// Exchange the authorization `code` for tokens (PKCE `code_verifier`, RFC 8707
/// `resource`). Public client → no secret.
async fn exchange_code(
    http: &reqwest::Client,
    params: &ExchangeParams<'_>,
    code: &str,
    verifier: &str,
    now: u64,
) -> Result<OAuthToken> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", params.redirect_uri),
        ("client_id", params.client_id),
        ("code_verifier", verifier),
        ("resource", params.resource),
    ];
    let resp = http.post(params.token_endpoint).form(&form).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token exchange failed: {status}: {body}");
    }
    let parsed: TokenResponse = resp.json().await.context("token response is not JSON")?;
    Ok(parsed.into_token(None, now))
}

// ---------------------------------------------------------------------------
// Interactive login flow
// ---------------------------------------------------------------------------

/// Run the full interactive login and return the credential to persist.
/// `open_url` is the caller's hook to present the authorization URL (the CLI
/// prints it and best-effort opens a browser) — this crate never touches the
/// terminal, and reqwest stays inside this crate (not the CLI). Discovery →
/// client_id (preconfigured or DCR) → loopback callback → code exchange.
/// `configured_scopes` overrides the scopes advertised by discovery when
/// non-empty.
pub async fn login(
    server_url: &str,
    preconfigured_client_id: Option<&str>,
    configured_scopes: &[String],
    open_url: impl FnOnce(&str) -> Result<()>,
) -> Result<LoginOutcome> {
    let http = &reqwest::Client::new();
    let (meta, prm_scopes) = discover(http, server_url).await?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("cannot bind the loopback OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");

    let client_id = match preconfigured_client_id {
        Some(id) => id.to_string(),
        None => {
            let registration_endpoint = meta.registration_endpoint.as_deref().context(
                "server offers no registration_endpoint and no client_id was configured; \
                 set oauth_client_id in [mcp.servers.<name>]",
            )?;
            register_client(http, registration_endpoint, &redirect_uri).await?
        }
    };

    let pkce = generate_pkce()?;
    let state = random_token()?;
    let scope = choose_scope(configured_scopes, &prm_scopes, &meta.scopes_supported);
    let authorize_url = build_authorize_url(
        &meta.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &pkce.challenge,
        &state,
        scope.as_deref(),
        server_url,
    )?;

    open_url(&authorize_url)?;
    let code = await_callback(listener, &state, CALLBACK_TIMEOUT).await?;

    let token = exchange_code(
        http,
        &ExchangeParams {
            token_endpoint: &meta.token_endpoint,
            redirect_uri: &redirect_uri,
            client_id: &client_id,
            resource: server_url,
        },
        &code,
        &pkce.verifier,
        now_unix(),
    )
    .await?;

    Ok(LoginOutcome {
        token,
        client_id,
        token_endpoint: meta.token_endpoint,
        resource: server_url.to_string(),
    })
}

/// Two-step discovery, falling back to the server's own origin as the issuer
/// when the protected-resource step fails (some servers are their own AS).
async fn discover(
    http: &reqwest::Client,
    server_url: &str,
) -> Result<(AuthServerMetadata, Option<Vec<String>>)> {
    let (issuer, prm_scopes) = match discover_prm_url(http, server_url).await {
        Ok(prm_url) => match fetch_prm(http, &prm_url).await {
            Ok(pair) => pair,
            Err(_) => (origin_of(server_url)?, None),
        },
        Err(_) => (origin_of(server_url)?, None),
    };
    let meta = fetch_as_metadata(http, &issuer).await?;
    Ok((meta, prm_scopes))
}

/// Configured scopes win; else the protected-resource's advertised scopes; else
/// the authorization server's. `None` lets the server apply its default.
fn choose_scope(
    configured: &[String],
    prm_scopes: &Option<Vec<String>>,
    as_scopes: &Option<Vec<String>>,
) -> Option<String> {
    for scopes in [
        Some(configured),
        prm_scopes.as_deref(),
        as_scopes.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !scopes.is_empty() {
            return Some(scopes.join(" "));
        }
    }
    None
}

fn origin_of(url: &str) -> Result<String> {
    Ok(Url::parse(url)
        .with_context(|| format!("invalid url '{url}'"))?
        .origin()
        .ascii_serialization())
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Request-time session: bearer injection + refresh
// ---------------------------------------------------------------------------

/// Holds a server's live OAuth token and refreshes it. Shared (one per server,
/// across every thread) behind the [`HttpTransport`](crate); `refresh_lock`
/// serializes refreshes so a burst of concurrent 401s refreshes once, and
/// `on_update` lets the CLI persist a rotated token back to disk.
pub struct OAuthSession {
    http: reqwest::Client,
    token_endpoint: String,
    client_id: String,
    resource: String,
    token: Mutex<OAuthToken>,
    refresh_lock: tokio::sync::Mutex<()>,
    on_update: Box<dyn Fn(&OAuthToken) + Send + Sync>,
    needs_relogin: AtomicBool,
}

impl OAuthSession {
    /// `on_update` is invoked (under no lock) with every refreshed token so the
    /// caller can persist it.
    pub fn new(
        token_endpoint: String,
        client_id: String,
        resource: String,
        token: OAuthToken,
        on_update: Box<dyn Fn(&OAuthToken) + Send + Sync>,
    ) -> Self {
        OAuthSession {
            http: reqwest::Client::new(),
            token_endpoint,
            client_id,
            resource,
            token: Mutex::new(token),
            refresh_lock: tokio::sync::Mutex::new(()),
            on_update,
            needs_relogin: AtomicBool::new(false),
        }
    }

    /// The access token to send now, refreshing first if it's within the skew
    /// of expiring.
    pub async fn bearer(&self) -> Result<String> {
        let access = self.token.lock().unwrap().access_token.clone();
        if near_expiry(&self.token.lock().unwrap(), now_unix()) {
            self.refresh_if_current(&access).await?;
        }
        Ok(self.token.lock().unwrap().access_token.clone())
    }

    /// A request that carried `used_bearer` got a 401: refresh once (unless a
    /// concurrent caller already moved past that token) so the caller can
    /// replay with the fresh one.
    pub async fn refresh_after_401(&self, used_bearer: &str) -> Result<()> {
        self.refresh_if_current(used_bearer).await
    }

    pub fn needs_relogin(&self) -> bool {
        self.needs_relogin.load(Ordering::Relaxed)
    }

    /// Refresh only if the live access token still equals `stale` — the dedup
    /// that turns a herd of concurrent 401s (all holding the same token) into a
    /// single refresh.
    async fn refresh_if_current(&self, stale: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        if self.token.lock().unwrap().access_token != stale {
            return Ok(());
        }
        self.do_refresh().await
    }

    async fn do_refresh(&self) -> Result<()> {
        let refresh_token = self.token.lock().unwrap().refresh_token.clone();
        let Some(refresh_token) = refresh_token else {
            self.needs_relogin.store(true, Ordering::Relaxed);
            bail!("no refresh token available; re-run `kloop mcp login`");
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", self.client_id.as_str()),
            ("resource", self.resource.as_str()),
        ];
        let resp = self
            .http
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await;
        let resp = match resp {
            Ok(resp) => resp,
            Err(e) => bail!("token refresh request failed: {e}"),
        };
        if !resp.status().is_success() {
            // invalid_grant (revoked/expired refresh token) and friends are not
            // recoverable by retrying — clear and demand a fresh login.
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            self.needs_relogin.store(true, Ordering::Relaxed);
            bail!("token refresh failed: {status}: {body}; re-run `kloop mcp login`");
        }
        let parsed: TokenResponse = resp
            .json()
            .await
            .context("token refresh response is not JSON")?;
        let refreshed = parsed.into_token(Some(refresh_token), now_unix());
        *self.token.lock().unwrap() = refreshed.clone();
        (self.on_update)(&refreshed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use wiremock::matchers::body_string_contains;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    #[test]
    fn parse_resource_metadata_handles_quoted_bare_and_absent() {
        assert_eq!(
            parse_resource_metadata(
                "Bearer resource_metadata=\"https://h/.well-known/oauth-protected-resource\""
            ),
            Some("https://h/.well-known/oauth-protected-resource".to_string())
        );
        // Bare token, other params present and ignored.
        assert_eq!(
            parse_resource_metadata("Bearer realm=\"x\", resource_metadata=https://h/prm error=y"),
            Some("https://h/prm".to_string())
        );
        assert_eq!(parse_resource_metadata("Bearer realm=\"x\""), None);
    }

    #[test]
    fn as_metadata_candidates_are_path_aware() {
        // Path-scoped issuer: path-aware form first, legacy nested second.
        assert_eq!(
            as_metadata_candidates("https://as.example.com/tenant1").unwrap(),
            vec![
                "https://as.example.com/.well-known/oauth-authorization-server/tenant1",
                "https://as.example.com/tenant1/.well-known/oauth-authorization-server",
                "https://as.example.com/.well-known/openid-configuration/tenant1",
                "https://as.example.com/tenant1/.well-known/openid-configuration",
            ]
        );
        // Bare origin: just the two roots.
        assert_eq!(
            as_metadata_candidates("https://as.example.com").unwrap(),
            vec![
                "https://as.example.com/.well-known/oauth-authorization-server",
                "https://as.example.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let pkce = generate_pkce().unwrap();
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expect);
        // base64url, no padding.
        assert!(!pkce.challenge.contains('='));
        assert!(!pkce.verifier.contains('='));
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_resource() {
        let url = build_authorize_url(
            "https://as.example.com/authorize",
            "client-123",
            "http://127.0.0.1:5000/callback",
            "chal",
            "state-xyz",
            Some("read write"),
            "https://mcp.example.com/mcp",
        )
        .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "client-123");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000/callback");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "state-xyz");
        assert_eq!(q["scope"], "read write");
        assert_eq!(q["resource"], "https://mcp.example.com/mcp");
    }

    #[test]
    fn into_token_makes_expiry_absolute_and_keeps_prev_refresh() {
        let resp = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(3600),
            scope: Some("s".into()),
        };
        let token = resp.into_token(Some("old-refresh".into()), 1000);
        assert_eq!(token.expires_at, Some(4600));
        assert_eq!(token.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[test]
    fn near_expiry_respects_skew_and_missing_expiry() {
        let with_exp = |exp| OAuthToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: exp,
            scope: None,
        };
        assert!(near_expiry(&with_exp(Some(1000)), 1000 - REFRESH_SKEW_SECS));
        assert!(!near_expiry(
            &with_exp(Some(1000)),
            1000 - REFRESH_SKEW_SECS - 1
        ));
        // No expiry advertised ⇒ never proactively refreshed.
        assert!(!near_expiry(&with_exp(None), u64::MAX));
    }

    #[tokio::test]
    async fn discovery_walks_prm_then_path_aware_as_metadata() {
        let server = MockServer::start().await;
        let base = server.uri();
        // Unauthenticated GET to the server URL → 401 pointing at the PRM.
        Mock::given(method("GET"))
            .and(path("/mcp"))
            .respond_with(
                ResponseTemplate::new(401).insert_header(
                    "www-authenticate",
                    format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                    )
                    .as_str(),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "authorization_servers": [format!("{base}/tenant")],
                "scopes_supported": ["mcp.read"],
            })))
            .mount(&server)
            .await;
        // Only the path-aware variant is served (the legacy nested one 404s).
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server/tenant"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
            })))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let (meta, scopes) = discover(&http, &format!("{base}/mcp")).await.unwrap();
        assert_eq!(meta.authorization_endpoint, format!("{base}/authorize"));
        assert_eq!(meta.token_endpoint, format!("{base}/token"));
        assert_eq!(meta.registration_endpoint, Some(format!("{base}/register")));
        assert_eq!(scopes, Some(vec!["mcp.read".to_string()]));
    }

    #[tokio::test]
    async fn dynamic_registration_returns_client_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .and(body_string_contains("authorization_code"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "client_id": "dcr-client-1",
            })))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let id = register_client(
            &http,
            &format!("{}/register", server.uri()),
            "http://127.0.0.1:9/callback",
        )
        .await
        .unwrap();
        assert_eq!(id, "dcr-client-1");
    }

    #[tokio::test]
    async fn callback_returns_code_on_matching_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .get(format!(
                    "http://127.0.0.1:{port}/callback?code=the-code&state=st"
                ))
                .send()
                .await;
        });
        let code = await_callback(listener, "st", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "the-code");
    }

    #[tokio::test]
    async fn callback_rejects_mismatched_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .get(format!(
                    "http://127.0.0.1:{port}/callback?code=c&state=WRONG"
                ))
                .send()
                .await;
        });
        let err = await_callback(listener, "expected", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("state mismatch"), "got: {err}");
    }

    #[tokio::test]
    async fn callback_times_out() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let err = await_callback(listener, "s", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn token_exchange_sends_verifier_and_resource() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("code_verifier=ver"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("resource="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let token = exchange_code(
            &http,
            &ExchangeParams {
                token_endpoint: &format!("{}/token", server.uri()),
                redirect_uri: "http://127.0.0.1:9/callback",
                client_id: "client-1",
                resource: "https://mcp.example.com/mcp",
            },
            "the-code",
            "ver",
            1000,
        )
        .await
        .unwrap();
        assert_eq!(token.access_token, "at-1");
        assert_eq!(token.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(token.expires_at, Some(4600));
    }

    /// Build a session whose token is already expired, so `bearer()` refreshes
    /// proactively; assert the new token is returned and persisted.
    #[tokio::test]
    async fn session_refreshes_expired_token_and_persists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt-old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-new",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;
        let saved: Arc<Mutex<Option<OAuthToken>>> = Arc::new(Mutex::new(None));
        let saved2 = saved.clone();
        let session = OAuthSession::new(
            format!("{}/token", server.uri()),
            "client-1".into(),
            "https://mcp.example.com/mcp".into(),
            OAuthToken {
                access_token: "at-old".into(),
                refresh_token: Some("rt-old".into()),
                expires_at: Some(0), // already expired
                scope: None,
            },
            Box::new(move |t| *saved2.lock().unwrap() = Some(t.clone())),
        );
        let bearer = session.bearer().await.unwrap();
        assert_eq!(bearer, "at-new");
        // Refresh with no rotated refresh_token keeps the old one.
        let persisted = saved.lock().unwrap().clone().unwrap();
        assert_eq!(persisted.access_token, "at-new");
        assert_eq!(persisted.refresh_token.as_deref(), Some("rt-old"));
        assert!(!session.needs_relogin());
    }

    #[tokio::test]
    async fn session_refresh_failure_marks_relogin() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
            })))
            .mount(&server)
            .await;
        let session = OAuthSession::new(
            format!("{}/token", server.uri()),
            "client-1".into(),
            "https://mcp.example.com/mcp".into(),
            OAuthToken {
                access_token: "at-old".into(),
                refresh_token: Some("rt-bad".into()),
                expires_at: Some(0),
                scope: None,
            },
            Box::new(|_| {}),
        );
        let err = session.bearer().await.unwrap_err();
        assert!(err.to_string().contains("invalid_grant"), "got: {err}");
        assert!(session.needs_relogin());
    }

    /// Full login over a mock AS: discovery + DCR + token exchange, with the
    /// `open_url` hook driving the loopback callback (as a browser would).
    #[tokio::test]
    async fn login_end_to_end_via_mock_server() {
        let server = MockServer::start().await;
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/mcp"))
            .respond_with(
                ResponseTemplate::new(401).insert_header(
                    "www-authenticate",
                    format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                    )
                    .as_str(),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "authorization_servers": [base.clone()],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"client_id": "dcr-42"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("client_id=dcr-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-final",
                "refresh_token": "rt-final",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        // The "browser": parse the port from the authorize URL's redirect_uri
        // and hit the loopback callback with the matching state.
        let open = |authorize_url: &str| -> Result<()> {
            let url = Url::parse(authorize_url).unwrap();
            let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let redirect = Url::parse(&q["redirect_uri"]).unwrap();
            let port = redirect.port().unwrap();
            let state = q["state"].clone();
            tokio::spawn(async move {
                let _ = reqwest::Client::new()
                    .get(format!(
                        "http://127.0.0.1:{port}/callback?code=auth-code&state={state}"
                    ))
                    .send()
                    .await;
            });
            Ok(())
        };
        let outcome = login(&format!("{base}/mcp"), None, &[], open)
            .await
            .unwrap();
        assert_eq!(outcome.client_id, "dcr-42");
        assert_eq!(outcome.token.access_token, "at-final");
        assert_eq!(outcome.token.refresh_token.as_deref(), Some("rt-final"));
        assert_eq!(outcome.token_endpoint, format!("{base}/token"));
        assert_eq!(outcome.resource, format!("{base}/mcp"));
    }
}
