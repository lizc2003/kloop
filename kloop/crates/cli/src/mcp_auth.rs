//! MCP remote OAuth, CLI side (plan 34b): the on-disk token store, the
//! `kloop mcp login <name>` flow (drive [`kloop_mcp::oauth::login`], open the
//! browser, persist), and building an [`OAuthSession`] from a stored token for
//! connect time. The wire protocol (PKCE/discovery/token exchange/refresh) lives
//! in `kloop-mcp`; this module owns everything with a config-file or terminal
//! side-effect, so reqwest never enters the CLI.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use kloop_mcp::oauth::login;
use kloop_mcp::oauth::LoginOutcome;
use kloop_mcp::oauth::OAuthSession;
use kloop_mcp::oauth::OAuthToken;

use crate::mcp::load_mcp_servers;
use crate::mcp::McpTransport;
use crate::startup::PROJECT_CONFIG;

/// Where OAuth tokens live: alongside the config, one file for all servers.
const OAUTH_STORE_PATH: &str = ".kloop/mcp-oauth.json";

/// One stored credential. `resource` doubles as the RFC 8707 audience the
/// refresh binds to and the server URL the store key is derived from; the token
/// fields flatten in so the JSON is one flat object per server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StoredCredential {
    client_id: String,
    token_endpoint: String,
    resource: String,
    #[serde(flatten)]
    token: OAuthToken,
}

/// The whole file: a version tag plus credentials keyed by `name|hash(url)`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    credentials: BTreeMap<String, StoredCredential>,
}

/// Read/modify/write access to the token file. Cheap (just holds the path); each
/// operation re-reads so a refresh from one place doesn't clobber another's
/// write. One process, so the in-file races two references guard with a file
/// lock are out of scope (noted in the plan).
#[derive(Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new(path: PathBuf) -> Self {
        CredentialStore { path }
    }

    pub fn default_path() -> Self {
        CredentialStore::new(PathBuf::from(OAUTH_STORE_PATH))
    }

    fn read(&self) -> Result<StoreFile> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("cannot parse {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", self.path.display())),
        }
    }

    fn write(&self, file: &StoreFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(file)?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        restrict_permissions(&self.path)?;
        Ok(())
    }

    /// Persist a fresh login, replacing any prior credential for this server.
    fn save(&self, name: &str, outcome: LoginOutcome) -> Result<()> {
        let mut file = self.read()?;
        file.version = 1;
        file.credentials.insert(
            credential_key(name, &outcome.resource),
            StoredCredential {
                client_id: outcome.client_id,
                token_endpoint: outcome.token_endpoint,
                resource: outcome.resource,
                token: outcome.token,
            },
        );
        self.write(&file)
    }

    fn get(&self, name: &str, url: &str) -> Option<StoredCredential> {
        self.read()
            .ok()?
            .credentials
            .remove(&credential_key(name, url))
    }

    /// Write back a refreshed token (the [`OAuthSession`] callback). Missing key
    /// = the file changed under us; nothing to update.
    fn update_token(&self, name: &str, url: &str, token: &OAuthToken) -> Result<()> {
        let mut file = self.read()?;
        if let Some(cred) = file.credentials.get_mut(&credential_key(name, url)) {
            cred.token = token.clone();
            self.write(&file)?;
        }
        Ok(())
    }

    /// Build a request-time [`OAuthSession`] for a logged-in server, or `None`
    /// if there's no stored token. The session's refresh callback writes the
    /// rotated token back here.
    pub fn session_for(self: &Arc<Self>, name: &str, url: &str) -> Option<Arc<OAuthSession>> {
        let cred = self.get(name, url)?;
        let store = Arc::clone(self);
        let name = name.to_string();
        let url = url.to_string();
        let on_update = Box::new(move |token: &OAuthToken| {
            if let Err(e) = store.update_token(&name, &url, token) {
                eprintln!(
                    "\x1b[2m[warning: could not persist refreshed OAuth token: {e:#}]\x1b[0m"
                );
            }
        });
        Some(Arc::new(OAuthSession::new(
            cred.token_endpoint,
            cred.client_id,
            cred.resource,
            cred.token,
            on_update,
        )))
    }
}

/// `name|<16 hex of sha256(url)>` — including the URL hash so re-pointing a
/// server's URL leaves its old token unreachable (audience changed), matching
/// both references.
fn credential_key(name: &str, url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("{name}|{}", hex16(&digest))
}

fn hex16(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot chmod {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// `kloop mcp login <name>`: resolve the server's OAuth config, run the
/// interactive flow (browser + loopback callback), and persist the token.
pub async fn run_login(server_name: &str) -> Result<()> {
    let servers = load_mcp_servers(Path::new(PROJECT_CONFIG))?;
    let server = servers
        .iter()
        .find(|s| s.name == server_name)
        .with_context(|| format!("no [mcp.servers.{server_name}] in {PROJECT_CONFIG}"))?;
    let (url, client_id, scopes) = match &server.transport {
        McpTransport::Http {
            url,
            bearer_token_env_var,
            oauth_client_id,
            oauth_scopes,
            ..
        } => {
            if bearer_token_env_var.is_some() {
                bail!(
                    "server '{server_name}' uses a static bearer_token_env_var, not OAuth; \
                     no login needed"
                );
            }
            (url.clone(), oauth_client_id.clone(), oauth_scopes.clone())
        }
        McpTransport::Stdio { .. } => {
            bail!("server '{server_name}' is a stdio (local) server; OAuth applies to remote (url) servers")
        }
    };

    println!("Logging in to MCP server '{server_name}' ({url})…");
    let outcome = login(&url, client_id.as_deref(), &scopes, |authorize_url| {
        println!("\nAuthorize kloop in your browser:\n\n  {authorize_url}\n");
        open_browser(authorize_url);
        println!("Waiting for the authorization redirect…");
        Ok(())
    })
    .await?;

    let store = CredentialStore::default_path();
    store.save(server_name, outcome)?;
    println!("\n✓ Logged in to '{server_name}'. Token saved to {OAUTH_STORE_PATH}.");
    Ok(())
}

/// Best-effort browser open. The URL is always printed too, so a failure here
/// (headless box, no handler) is harmless — the user pastes it.
fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin; the empty arg is the window title.
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(tag: &str) -> (CredentialStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kloop-oauth-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp-oauth.json");
        let _ = std::fs::remove_file(&path);
        (CredentialStore::new(path.clone()), path)
    }

    fn outcome(access: &str, refresh: &str) -> LoginOutcome {
        LoginOutcome {
            token: OAuthToken {
                access_token: access.into(),
                refresh_token: Some(refresh.into()),
                expires_at: Some(9999),
                scope: Some("s".into()),
            },
            client_id: "client-1".into(),
            token_endpoint: "https://as/token".into(),
            resource: "https://mcp.example.com/mcp".into(),
        }
    }

    #[test]
    fn key_depends_on_both_name_and_url() {
        let a = credential_key("gh", "https://a/mcp");
        assert_ne!(a, credential_key("gh", "https://b/mcp"));
        assert_ne!(a, credential_key("other", "https://a/mcp"));
        // Stable for the same inputs.
        assert_eq!(a, credential_key("gh", "https://a/mcp"));
    }

    #[test]
    fn save_then_get_round_trips_and_url_change_invalidates() {
        let (store, path) = store_in("roundtrip");
        store.save("gh", outcome("at-1", "rt-1")).unwrap();
        let got = store.get("gh", "https://mcp.example.com/mcp").unwrap();
        assert_eq!(got.client_id, "client-1");
        assert_eq!(got.token.access_token, "at-1");
        // A different URL for the same name has no entry (token invalidated).
        assert!(store.get("gh", "https://mcp.example.com/other").is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn update_token_rewrites_only_the_token_fields() {
        let (store, path) = store_in("update");
        store.save("gh", outcome("at-1", "rt-1")).unwrap();
        let refreshed = OAuthToken {
            access_token: "at-2".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: Some(12345),
            scope: Some("s".into()),
        };
        store
            .update_token("gh", "https://mcp.example.com/mcp", &refreshed)
            .unwrap();
        let got = store.get("gh", "https://mcp.example.com/mcp").unwrap();
        assert_eq!(got.token, refreshed);
        assert_eq!(got.client_id, "client-1"); // untouched
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn session_for_is_none_without_a_token() {
        let (store, path) = store_in("none");
        let store = Arc::new(store);
        assert!(store
            .session_for("gh", "https://mcp.example.com/mcp")
            .is_none());
        store.save("gh", outcome("at-1", "rt-1")).unwrap();
        assert!(store
            .session_for("gh", "https://mcp.example.com/mcp")
            .is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn stored_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (store, path) = store_in("perms");
        store.save("gh", outcome("at-1", "rt-1")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
