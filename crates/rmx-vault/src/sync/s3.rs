//! S3-compatible storage with conditional writes.

use std::fmt;
use std::time::Duration;

use rusty_s3::actions::{GetObject, HeadObject, PutObject, S3Action};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use ureq::Agent;
use ureq::http::Response;
use url::Url;

use super::{RemoteObject, RemoteVersion, SyncBackend, http};
use crate::error::BackendError;
use crate::secret::Secret;

const SIGNATURE_LIFETIME: Duration = Duration::from_secs(300);

/// Connection details for [`S3Backend`].
#[derive(Clone)]
pub struct S3Config {
    /// Service endpoint, such as `https://s3.eu-west-1.amazonaws.com`.
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// Signing region.
    pub region: String,
    /// Object key of the vault.
    pub key: String,
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: Secret,
    /// Use `endpoint/bucket/key` URLs instead of `bucket.endpoint/key`; needed by most
    /// self-hosted services.
    pub path_style: bool,
    /// Permit plain HTTP to non-loopback hosts.
    pub allow_plain_http: bool,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Syncs through one object using `If-Match` / `If-None-Match` on `PutObject`.
///
/// The service must support conditional writes, as AWS S3 and current MinIO do.
pub struct S3Backend {
    agent: Agent,
    bucket: Bucket,
    credentials: Credentials,
    key: String,
}

impl fmt::Debug for S3Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Backend").field("key", &self.key).finish_non_exhaustive()
    }
}

impl S3Backend {
    /// Validates `config`. No request is made yet.
    pub fn new(config: S3Config) -> Result<Self, BackendError> {
        let endpoint = Url::parse(&config.endpoint)
            .map_err(|_| BackendError::Protocol("invalid S3 endpoint".to_owned()))?;
        http::check_url(&endpoint, config.allow_plain_http)?;
        let style = if config.path_style { UrlStyle::Path } else { UrlStyle::VirtualHost };
        let bucket = Bucket::new(endpoint, style, config.bucket, config.region)
            .map_err(|_| BackendError::Protocol("invalid S3 bucket or endpoint".to_owned()))?;
        Ok(Self {
            agent: http::agent(),
            bucket,
            credentials: Credentials::new(config.access_key, config.secret_key.expose()),
            key: config.key,
        })
    }

    fn version(response: &Response<ureq::Body>) -> Result<RemoteVersion, BackendError> {
        http::etag(response)
            .map(RemoteVersion)
            .ok_or_else(|| BackendError::Protocol("S3 response had no ETag".to_owned()))
    }
}

impl SyncBackend for S3Backend {
    fn id(&self) -> String {
        format!("s3:{}/{}", self.bucket.object_url("").as_str().trim_end_matches('/'), self.key)
    }

    fn head(&self) -> Result<Option<RemoteVersion>, BackendError> {
        let url = HeadObject::new(&self.bucket, Some(&self.credentials), &self.key)
            .sign(SIGNATURE_LIFETIME);
        let response = self.agent.head(url.as_str()).call().map_err(http::transport)?;
        match response.status().as_u16() {
            200..=299 => Self::version(&response).map(Some),
            404 => Ok(None),
            status => Err(BackendError::Http(status)),
        }
    }

    fn get(&self) -> Result<Option<RemoteObject>, BackendError> {
        let url = GetObject::new(&self.bucket, Some(&self.credentials), &self.key)
            .sign(SIGNATURE_LIFETIME);
        let mut response = self.agent.get(url.as_str()).call().map_err(http::transport)?;
        match response.status().as_u16() {
            200..=299 => {
                let version = Self::version(&response)?;
                Ok(Some(RemoteObject { data: http::body(&mut response)?, version }))
            },
            404 => Ok(None),
            status => Err(BackendError::Http(status)),
        }
    }

    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError> {
        let (header, value) = match expected {
            Some(version) => ("if-match", version.0.as_str()),
            None => ("if-none-match", "*"),
        };
        let mut action = PutObject::new(&self.bucket, Some(&self.credentials), &self.key);
        action.headers_mut().insert(header, value);
        let url = action.sign(SIGNATURE_LIFETIME);
        let response = self
            .agent
            .put(url.as_str())
            .header(header, value)
            .send(data)
            .map_err(http::transport)?;
        match response.status().as_u16() {
            200..=299 => Ok(http::etag(&response).map(RemoteVersion)),
            // 409 is S3's "concurrent conditional write"; 404 means an expected object vanished.
            409 | 412 => Err(BackendError::Conflict),
            404 if expected.is_some() => Err(BackendError::Conflict),
            status => Err(BackendError::Http(status)),
        }
    }
}
