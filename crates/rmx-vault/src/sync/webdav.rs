//! WebDAV with `If-Match` compare-and-swap.

use std::fmt;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ureq::Agent;
use ureq::http::Response;
use url::Url;

use super::http;
use super::{RemoteObject, RemoteVersion, SyncBackend};
use crate::error::BackendError;
use crate::secret::Secret;

const WEAK_ETAG_RETRIES: u32 = 3;
const WEAK_ETAG_WAIT: Duration = Duration::from_millis(1100);

/// Connection details for [`WebDavBackend`].
#[derive(Clone)]
pub struct WebDavConfig {
    /// Full URL of the vault file. Its parent collection must exist.
    pub url: String,
    /// Basic-auth user name; `None` sends no credentials.
    pub username: Option<String>,
    /// Basic-auth password.
    pub password: Secret,
    /// Permit plain HTTP to non-loopback hosts, exposing the login (never the vault's contents).
    pub allow_plain_http: bool,
}

impl fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("allow_plain_http", &self.allow_plain_http)
            .finish_non_exhaustive()
    }
}

/// Syncs through one file on a WebDAV server.
///
/// The server must honor `If-Match` and report strong ETags, as Apache `mod_dav` does and
/// Nextcloud is documented to. A server that ignores `If-Match` cannot be detected and would lose concurrent pushes.
pub struct WebDavBackend {
    agent: Agent,
    url: Url,
    authorization: Option<Secret>,
}

impl fmt::Debug for WebDavBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavBackend").field("url", &self.url.as_str()).finish_non_exhaustive()
    }
}

impl WebDavBackend {
    /// Validates `config`. No request is made yet.
    pub fn new(config: WebDavConfig) -> Result<Self, BackendError> {
        let url = Url::parse(&config.url)
            .map_err(|_| BackendError::Protocol("invalid WebDAV URL".to_owned()))?;
        http::check_url(&url, config.allow_plain_http)?;
        let authorization = config.username.map(|user| {
            let login = Secret::new(format!("{user}:{}", config.password.expose()));
            Secret::new(format!("Basic {}", STANDARD.encode(login.expose())))
        });
        Ok(Self { agent: http::agent(), url, authorization })
    }

    fn with_auth<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        match &self.authorization {
            Some(value) => request.header("authorization", value.expose()),
            None => request,
        }
    }

    /// Issues a request until its ETag is strong, or `None` if the file does not exist.
    ///
    /// Apache marks an ETag weak while the file's modification time is within a second of the
    /// request, and weak tags never satisfy `If-Match`, so that second has to be waited out.
    fn fetch_strong(
        request: impl Fn() -> Result<Response<ureq::Body>, BackendError>,
    ) -> Result<Option<(Response<ureq::Body>, RemoteVersion)>, BackendError> {
        for attempt in 0..=WEAK_ETAG_RETRIES {
            let response = request()?;
            match response.status().as_u16() {
                200..=299 => {},
                404 => return Ok(None),
                status => return Err(Self::status_error(status)),
            }
            match http::etag(&response) {
                Some(tag) if !tag.starts_with("W/") => {
                    return Ok(Some((response, RemoteVersion(tag))));
                },
                Some(_) if attempt < WEAK_ETAG_RETRIES => std::thread::sleep(WEAK_ETAG_WAIT),
                _ => break,
            }
        }
        Err(BackendError::Protocol(
            "WebDAV server did not report a strong ETag; safe sync is impossible".to_owned(),
        ))
    }

    fn status_error(status: u16) -> BackendError {
        match status {
            409 => BackendError::Protocol("WebDAV parent collection does not exist".to_owned()),
            other => BackendError::Http(other),
        }
    }
}

impl SyncBackend for WebDavBackend {
    fn id(&self) -> String {
        format!("webdav:{}", self.url)
    }

    fn head(&self) -> Result<Option<RemoteVersion>, BackendError> {
        let request =
            || self.with_auth(self.agent.head(self.url.as_str())).call().map_err(http::transport);
        Ok(Self::fetch_strong(request)?.map(|(_, version)| version))
    }

    fn get(&self) -> Result<Option<RemoteObject>, BackendError> {
        let request =
            || self.with_auth(self.agent.get(self.url.as_str())).call().map_err(http::transport);
        Self::fetch_strong(request)?
            .map(|(mut response, version)| {
                Ok(RemoteObject { data: http::body(&mut response)?, version })
            })
            .transpose()
    }

    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError> {
        let request = self.with_auth(self.agent.put(self.url.as_str()));
        let request = match expected {
            Some(version) => request.header("if-match", &version.0),
            None => request.header("if-none-match", "*"),
        };
        let response = request.send(data).map_err(http::transport)?;
        match response.status().as_u16() {
            200..=299 => {
                Ok(http::etag(&response).filter(|t| !t.starts_with("W/")).map(RemoteVersion))
            },
            412 | 423 => Err(BackendError::Conflict),
            status => Err(Self::status_error(status)),
        }
    }
}
