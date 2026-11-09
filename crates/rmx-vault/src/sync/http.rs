//! The blocking HTTP plumbing shared by WebDAV and S3.

use std::time::Duration;

use ureq::Agent;
use ureq::http::Response;

use crate::error::BackendError;

/// Upper bound on a downloaded vault.
const MAX_BODY: u64 = 1 << 30;

/// An agent that reports statuses instead of failing on them and never follows redirects, which
/// could carry credentials elsewhere.
pub(super) fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .new_agent()
}

/// Maps a transport failure without ever echoing a (possibly signed) URI.
pub(super) fn transport(e: ureq::Error) -> BackendError {
    match e {
        ureq::Error::BadUri(_) => BackendError::Protocol("invalid request URI".to_owned()),
        ureq::Error::Io(io) => BackendError::Io(io),
        other => BackendError::Transport(other.to_string()),
    }
}

pub(super) fn etag(response: &Response<ureq::Body>) -> Option<String> {
    response.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned)
}

pub(super) fn body(response: &mut Response<ureq::Body>) -> Result<Vec<u8>, BackendError> {
    response.body_mut().with_config().limit(MAX_BODY).read_to_vec().map_err(transport)
}

/// Rejects plain HTTP to anywhere but loopback unless explicitly allowed.
pub(super) fn check_url(url: &url::Url, allow_plain_http: bool) -> Result<(), BackendError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BackendError::Protocol("credentials must not be part of the URL".to_owned()));
    }
    let loopback = matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    match url.scheme() {
        "https" => Ok(()),
        "http" if allow_plain_http || loopback => Ok(()),
        "http" => Err(BackendError::Protocol("refusing plain HTTP to a remote host".to_owned())),
        _ => Err(BackendError::Protocol("unsupported URL scheme".to_owned())),
    }
}
