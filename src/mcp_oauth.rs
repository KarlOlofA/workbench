//! OAuth sign-in for remote MCP servers (for example Linear).
//!
//! `sign_in` runs the browser flow once: rmcp discovers the server's authorization server,
//! registers Workbench as a client, and exchanges the code that comes back to a one-shot
//! listener on 127.0.0.1. The tokens are kept in `mcp-oauth.json` in the user's config folder,
//! never in the project. Before each turn `with_tokens` adds `Authorization: Bearer <token>`
//! to every signed-in server (refreshing the token when it is about to expire), so every
//! provider gets the server as a plain URL with a header.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::transport::auth::OAuthState;
use rmcp::transport::{AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, StoredCredentials};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::mcp::{self, Server, Transport};

const TOKEN_FILE: &str = "mcp-oauth.json";
/// How long the user has to finish signing in in the browser.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// The tokens of one server, in a file that holds the tokens of every server by URL.
struct FileStore {
    path: PathBuf,
    url: String,
}

type Tokens = BTreeMap<String, StoredCredentials>;

fn read_tokens(path: &Path) -> Tokens {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write_tokens(path: &Path, tokens: &Tokens) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    // The file holds secrets, so only the user may read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl CredentialStore for FileStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        Ok(read_tokens(&self.path).remove(&self.url))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let mut tokens = read_tokens(&self.path);
        tokens.insert(self.url.clone(), credentials);
        write_tokens(&self.path, &tokens).map_err(AuthError::CredentialStoreError)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let mut tokens = read_tokens(&self.path);
        tokens.remove(&self.url);
        write_tokens(&self.path, &tokens).map_err(AuthError::CredentialStoreError)
    }
}

fn token_file() -> Result<PathBuf, String> {
    crate::config::config_path(TOKEN_FILE).ok_or_else(|| "no config folder (HOME is not set)".to_string())
}

/// The URL tokens are stored under, with `${VAR}` references filled in.
fn server_url(server: &Server) -> String {
    mcp::expand_env(server.url.trim())
}

async fn manager(path: PathBuf, url: &str) -> Result<AuthorizationManager, String> {
    let mut manager = AuthorizationManager::new(url).await.map_err(|e| e.to_string())?;
    manager.set_credential_store(FileStore { path, url: url.to_string() });
    Ok(manager)
}

pub fn is_signed_in(server: &Server) -> bool {
    token_file().is_ok_and(|path| read_tokens(&path).contains_key(&server_url(server)))
}

pub fn sign_out(server: &Server) -> Result<(), String> {
    let path = token_file()?;
    let mut tokens = read_tokens(&path);
    tokens.remove(&server_url(server));
    write_tokens(&path, &tokens)
}

/// Opens the server's sign-in page in the browser and waits until the browser comes back with a code.
pub async fn sign_in(server: &Server) -> Result<(), String> {
    let url = server_url(server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let base = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());

    let mut state = OAuthState::Unauthorized(manager(token_file()?, &url).await?);
    let request = AuthorizationRequest::new(format!("{base}/callback")).with_client_name("Workbench");
    state.start_authorization(request).await.map_err(|e| e.to_string())?;
    crate::platform::open_url(&state.get_authorization_url().await.map_err(|e| e.to_string())?);

    let path = tokio::time::timeout(SIGN_IN_TIMEOUT, receive_callback(&listener))
        .await
        .map_err(|_| "sign-in timed out".to_string())??;
    state.handle_callback_url(&format!("{base}{path}")).await.map_err(|e| e.to_string())
}

/// Accepts the browser's redirect, answers it, and returns its path with the query (`/callback?code=…`).
async fn receive_callback(listener: &tokio::net::TcpListener) -> Result<String, String> {
    let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
    let mut buf = vec![0; 8192];
    let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request_path(&request).ok_or("the browser sent an unexpected request")?.to_string();
    let page = "<html><body><h3>Signed in to the MCP server. You can close this tab and return to Workbench.</h3></body></html>";
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
    let _ = stream.write_all(response.as_bytes()).await;
    Ok(path)
}

/// The path of an HTTP GET request: `GET /callback?code=a HTTP/1.1` gives `/callback?code=a`.
fn request_path(request: &str) -> Option<&str> {
    let mut parts = request.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next()
}

/// The servers with `Authorization: Bearer <token>` added to each signed-in remote server.
/// A server whose token can't be loaded or refreshed is passed on unchanged, so its status shows the failure.
pub async fn with_tokens(servers: &[Server]) -> Vec<Server> {
    let Ok(path) = token_file() else { return servers.to_vec() };
    with_tokens_from(&path, servers).await
}

async fn with_tokens_from(path: &Path, servers: &[Server]) -> Vec<Server> {
    let stored = read_tokens(path);
    let mut out = servers.to_vec();
    for server in &mut out {
        let url = server_url(server);
        if server.transport == Transport::Stdio || !server.is_usable() || !stored.contains_key(&url) {
            continue;
        }
        if let Ok(token) = access_token(path, &url).await {
            server.headers.push(format!("Authorization: Bearer {token}"));
        }
    }
    out
}

async fn access_token(path: &Path, url: &str) -> Result<String, String> {
    let mut manager = manager(path.to_path_buf(), url).await?;
    manager.initialize_from_store().await.map_err(|e| e.to_string())?;
    manager.get_access_token().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wb-oauth-{name}-{}.json", std::process::id()))
    }

    fn credentials(client_id: &str) -> StoredCredentials {
        StoredCredentials::new(client_id.to_string(), None, Vec::new(), None)
    }

    #[tokio::test]
    async fn file_store_keeps_each_url_apart() {
        let path = test_file("store");
        let a = FileStore { path: path.clone(), url: "https://a/mcp".into() };
        let b = FileStore { path: path.clone(), url: "https://b/mcp".into() };
        a.save(credentials("client-a")).await.unwrap();
        b.save(credentials("client-b")).await.unwrap();
        a.clear().await.unwrap();
        assert!(a.load().await.unwrap().is_none());
        assert_eq!(b.load().await.unwrap().unwrap().client_id, "client-b");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_the_callback_path() {
        assert_eq!(request_path("GET /callback?code=a&state=b HTTP/1.1\r\nHost: x\r\n"), Some("/callback?code=a&state=b"));
        assert_eq!(request_path("POST /callback HTTP/1.1\r\n"), None);
        assert_eq!(request_path(""), None);
    }

    #[tokio::test]
    async fn servers_without_tokens_are_unchanged() {
        let servers = vec![
            Server { name: "files".into(), command: "files".into(), ..Default::default() },
            Server { name: "web".into(), transport: Transport::Http, url: "https://example.com/mcp".into(), ..Default::default() },
        ];
        assert_eq!(with_tokens_from(&test_file("none"), &servers).await, servers);
    }
}
