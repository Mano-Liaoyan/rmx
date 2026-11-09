use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{HashAlg, PublicKey};
use sha1::Sha1;

use crate::host_key::{HostKeyCheck, HostKeyDecision, HostKeyRejection, HostKeyVerifier};
use crate::pattern::pattern_list_matches;

/// The parsed contents of an OpenSSH `known_hosts` file.
///
/// Supports comma-separated host lists, `*` and `?` wildcards, `!` negation,
/// `[host]:port` entries, hashed (`|1|...`) entries and `@revoked` markers.
/// `@cert-authority` lines are ignored: this client does not negotiate host
/// certificates. Lines it cannot parse are skipped, as OpenSSH does.
#[derive(Clone, Debug, Default)]
pub struct KnownHosts {
    entries: Vec<Entry>,
}

#[derive(Clone, Debug)]
struct Entry {
    line: usize,
    hosts: Hosts,
    revoked: bool,
    key: PublicKey,
}

#[derive(Clone, Debug)]
enum Hosts {
    Patterns(Vec<String>),
    Hashed { salt: Vec<u8>, digest: Vec<u8> },
}

/// What a `known_hosts` file says about a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnownHostsStatus {
    /// The key is recorded for this host.
    Known,
    /// No key of this type is recorded for this host.
    Unknown,
    /// A different key of the same type is recorded for this host.
    Changed {
        /// One-based line number of the recorded key.
        line: usize,
        /// The recorded key.
        known: PublicKey,
    },
    /// The key is marked `@revoked` for this host.
    Revoked {
        /// One-based line number of the revocation.
        line: usize,
    },
}

impl KnownHosts {
    /// Parses the text of a `known_hosts` file.
    pub fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .enumerate()
            .filter_map(|(index, line)| parse_line(index + 1, line))
            .collect();
        Self { entries }
    }

    /// Checks `key` for `host` on `port`.
    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> KnownHostsStatus {
        let name = lookup_name(host, port);
        let matching: Vec<&Entry> =
            self.entries.iter().filter(|entry| entry.hosts.matches(&name)).collect();
        if let Some(entry) =
            matching.iter().find(|entry| entry.revoked && entry.key.key_data() == key.key_data())
        {
            return KnownHostsStatus::Revoked { line: entry.line };
        }
        let trusted = || matching.iter().filter(|entry| !entry.revoked);
        if trusted().any(|entry| entry.key.key_data() == key.key_data()) {
            return KnownHostsStatus::Known;
        }
        match trusted().find(|entry| entry.key.algorithm() == key.algorithm()) {
            Some(entry) => KnownHostsStatus::Changed { line: entry.line, known: entry.key.clone() },
            None => KnownHostsStatus::Unknown,
        }
    }
}

impl Hosts {
    fn matches(&self, name: &str) -> bool {
        match self {
            Hosts::Patterns(patterns) => {
                pattern_list_matches(patterns.iter().map(String::as_str), name)
            },
            Hosts::Hashed { salt, digest } => {
                let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(salt) else {
                    return false;
                };
                mac.update(name.as_bytes());
                mac.verify_slice(digest).is_ok()
            },
        }
    }
}

fn parse_line(line: usize, text: &str) -> Option<Entry> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('#') {
        return None;
    }
    let mut fields = text.split_whitespace();
    let mut hosts = fields.next()?;
    let mut revoked = false;
    if let Some(marker) = hosts.strip_prefix('@') {
        match marker {
            "revoked" => revoked = true,
            _ => return None,
        }
        hosts = fields.next()?;
    }
    let algorithm = fields.next()?;
    let key = fields.next()?;
    let key = PublicKey::from_openssh(&format!("{algorithm} {key}")).ok()?;
    Some(Entry { line, hosts: parse_hosts(hosts)?, revoked, key })
}

fn parse_hosts(field: &str) -> Option<Hosts> {
    if let Some(hashed) = field.strip_prefix("|1|") {
        let (salt, digest) = hashed.split_once('|')?;
        return Some(Hosts::Hashed {
            salt: BASE64.decode(salt.as_bytes()).ok()?,
            digest: BASE64.decode(digest.as_bytes()).ok()?,
        });
    }
    Some(Hosts::Patterns(field.split(',').map(str::to_ascii_lowercase).collect()))
}

/// The name a host is filed under: the bare host on port 22, otherwise
/// `[host]:port`. Hashed lines were hashed over a lowercase name.
fn lookup_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 { host } else { format!("[{host}]:{port}") }
}

/// What [`KnownHostsVerifier`] does with a host the file has no key for.
#[derive(Clone)]
pub enum UnknownHostPolicy {
    /// Refuse the connection.
    Reject,
    /// Accept the key and add it to the file.
    AcceptNew,
    /// Ask this verifier, typically a UI prompt. If it accepts, the key is
    /// added to the file.
    Ask(Arc<dyn HostKeyVerifier>),
}

/// A [`HostKeyVerifier`] backed by an OpenSSH `known_hosts` file.
///
/// A key that differs from the recorded one, or that is `@revoked`, is always
/// rejected, whatever the [`UnknownHostPolicy`].
#[derive(Clone)]
pub struct KnownHostsVerifier {
    path: PathBuf,
    unknown: UnknownHostPolicy,
}

impl KnownHostsVerifier {
    /// Uses the file at `path`, which need not exist yet.
    pub fn new(path: impl Into<PathBuf>, unknown: UnknownHostPolicy) -> Self {
        Self { path: path.into(), unknown }
    }

    /// The user's `~/.ssh/known_hosts`, if the home directory is known.
    pub fn default_path() -> Option<PathBuf> {
        std::env::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
    }

    async fn record(&self, host: &str, port: u16, key: &PublicKey) {
        let path = self.path.clone();
        let name = lookup_name(host, port);
        let entry = key.to_openssh();
        let result = tokio::task::spawn_blocking(move || match entry {
            Ok(entry) => append_entry(&path, &format!("{name} {entry}")),
            Err(error) => Err(io::Error::other(error)),
        })
        .await;
        // The connection is approved either way; the cost of failing to save
        // is being asked again next time.
        match result {
            Ok(Ok(())) => {},
            Ok(Err(error)) => log::warn!("could not update known_hosts: {error}"),
            Err(error) => log::warn!("could not update known_hosts: {error}"),
        }
    }
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

/// Appends one line, creating the file (mode 0600) and its directory as needed.
fn append_entry(path: &Path, entry: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut line = String::with_capacity(entry.len() + 2);
    if ends_without_newline(&mut file)? {
        line.push('\n');
    }
    line.push_str(entry);
    line.push('\n');
    file.write_all(line.as_bytes())
}

fn ends_without_newline(file: &mut std::fs::File) -> io::Result<bool> {
    if file.seek(SeekFrom::End(0))? == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

#[async_trait]
impl HostKeyVerifier for KnownHostsVerifier {
    async fn verify(&self, check: &HostKeyCheck<'_>) -> HostKeyDecision {
        let path = self.path.clone();
        let text = match tokio::task::spawn_blocking(move || read_or_empty(&path)).await {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => {
                return HostKeyDecision::Reject(HostKeyRejection::Failed(format!(
                    "{}: {error}",
                    self.path.display()
                )));
            },
            Err(error) => {
                return HostKeyDecision::Reject(HostKeyRejection::Failed(error.to_string()));
            },
        };
        let location = |line: usize| format!("{}:{line}", self.path.display());
        match KnownHosts::parse(&text).check(check.host, check.port, check.key) {
            KnownHostsStatus::Known => HostKeyDecision::Accept,
            KnownHostsStatus::Revoked { line } => {
                HostKeyDecision::Reject(HostKeyRejection::Revoked { location: location(line) })
            },
            KnownHostsStatus::Changed { line, known } => {
                HostKeyDecision::Reject(HostKeyRejection::Changed {
                    known_fingerprint: known.fingerprint(HashAlg::Sha256).to_string(),
                    location: location(line),
                })
            },
            KnownHostsStatus::Unknown => {
                let decision = match &self.unknown {
                    UnknownHostPolicy::Reject => HostKeyDecision::Reject(HostKeyRejection::Unknown),
                    UnknownHostPolicy::AcceptNew => HostKeyDecision::Accept,
                    UnknownHostPolicy::Ask(verifier) => verifier.verify(check).await,
                };
                if decision == HostKeyDecision::Accept {
                    self.record(check.host, check.port, check.key).await;
                }
                decision
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMOeS+6t/fzP4cnALtqrw8Tt1mvOXSkQ6LUtGPdpS3CC";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKwQQ8ACfbYzgJdc/C0v/wXsw1uNLOMAKLkwTX9JdeBT";
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC+6WpUDlFwMtiQ7KMDl7ZGNihYJXEQi/pW\
ie4Gk2Jc6t7DdLOgK2WuiZqlHGexVQW9mqalz3N3E3PBcB0BW6lYC9G+NB4M6sxGgo83ELZlXBMfoYJLDZa3Pzh8S38CjOf1\
A06ee4ccgm0sXb4lL7q3YWrrjvLaFTu7gUqvcEN3qQ==";

    fn key(text: &str) -> PublicKey {
        PublicKey::from_openssh(text).expect("test key parses")
    }

    fn hosts(text: &str) -> KnownHosts {
        KnownHosts::parse(text)
    }

    /// Builds a hashed entry the way `ssh-keygen -H` does.
    fn hashed(name: &str, salt: &[u8]) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(salt).unwrap();
        mac.update(name.as_bytes());
        let digest = mac.finalize().into_bytes();
        format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(&digest))
    }

    #[test]
    fn plain_host_is_known() {
        let file = hosts(&format!("example.com {KEY_A}\n"));
        assert_eq!(file.check("example.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
    }

    #[test]
    fn host_lookup_is_case_insensitive() {
        let file = hosts(&format!("Example.COM {KEY_A}\n"));
        assert_eq!(file.check("EXAMPLE.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
    }

    #[test]
    fn non_default_port_uses_bracket_form() {
        let file = hosts(&format!("[example.com]:2222 {KEY_A}\nother.com {KEY_A}\n"));
        assert_eq!(file.check("example.com", 2222, &key(KEY_A)), KnownHostsStatus::Known);
        assert_eq!(file.check("example.com", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
        // A bare entry says nothing about other ports, as in OpenSSH.
        assert_eq!(file.check("other.com", 2222, &key(KEY_A)), KnownHostsStatus::Unknown);
    }

    #[test]
    fn ipv6_literal_with_port() {
        let file = hosts(&format!("[::1]:2222 {KEY_A}\n"));
        assert_eq!(file.check("::1", 2222, &key(KEY_A)), KnownHostsStatus::Known);
    }

    #[test]
    fn host_lists_wildcards_and_negation() {
        let file = hosts(&format!("a.com,*.example.com,!bad.example.com {KEY_A}\n"));
        assert_eq!(file.check("a.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
        assert_eq!(file.check("web.example.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
        assert_eq!(file.check("bad.example.com", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
        assert_eq!(file.check("elsewhere.net", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
    }

    #[test]
    fn hashed_entries_match_the_right_name_only() {
        let entry = hashed("secret.example.com", b"01234567890123456789");
        let file = hosts(&format!("{entry} {KEY_A}\n"));
        assert_eq!(file.check("secret.example.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
        assert_eq!(file.check("other.example.com", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
    }

    #[test]
    fn hashed_entries_cover_ports() {
        let entry = hashed("[secret.example.com]:2222", b"abcdefghijklmnopqrst");
        let file = hosts(&format!("{entry} {KEY_A}\n"));
        assert_eq!(file.check("secret.example.com", 2222, &key(KEY_A)), KnownHostsStatus::Known);
    }

    #[test]
    fn different_key_of_same_type_is_changed() {
        let file = hosts(&format!("# comment\n\nexample.com {KEY_A}\n"));
        let status = file.check("example.com", 22, &key(KEY_B));
        assert_eq!(status, KnownHostsStatus::Changed { line: 3, known: key(KEY_A) });
    }

    #[test]
    fn other_key_type_is_unknown_not_changed() {
        let file = hosts(&format!("example.com {KEY_RSA}\n"));
        assert_eq!(file.check("example.com", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
    }

    #[test]
    fn any_matching_line_makes_a_key_known() {
        let file = hosts(&format!("example.com {KEY_A}\nexample.com {KEY_B}\n"));
        assert_eq!(file.check("example.com", 22, &key(KEY_B)), KnownHostsStatus::Known);
    }

    #[test]
    fn revoked_takes_precedence() {
        let file = hosts(&format!("example.com {KEY_A}\n@revoked example.com {KEY_A}\n"));
        assert_eq!(
            file.check("example.com", 22, &key(KEY_A)),
            KnownHostsStatus::Revoked { line: 2 }
        );
    }

    #[test]
    fn revoked_only_applies_to_its_host() {
        let file = hosts(&format!("@revoked example.com {KEY_A}\n"));
        assert_eq!(file.check("other.com", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
    }

    #[test]
    fn cert_authority_lines_grant_no_trust() {
        let file = hosts(&format!("@cert-authority example.com {KEY_A}\n"));
        assert_eq!(file.check("example.com", 22, &key(KEY_A)), KnownHostsStatus::Unknown);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let text = format!("garbage\nexample.com ssh-ed25519 notbase64!!\nexample.com {KEY_A}\n");
        assert_eq!(hosts(&text).check("example.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
    }

    #[test]
    fn comments_after_the_key_are_ignored() {
        let file = hosts(&format!("example.com {KEY_A} root@box and more\n"));
        assert_eq!(file.check("example.com", 22, &key(KEY_A)), KnownHostsStatus::Known);
    }

    fn check<'a>(host: &'a str, port: u16, key: &'a PublicKey) -> HostKeyCheck<'a> {
        HostKeyCheck { host, port, key }
    }

    fn temp_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("sub").join("known_hosts")
    }

    #[tokio::test]
    async fn reject_policy_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir);
        let verifier = KnownHostsVerifier::new(&path, UnknownHostPolicy::Reject);
        let k = key(KEY_A);
        assert_eq!(
            verifier.verify(&check("h", 22, &k)).await,
            HostKeyDecision::Reject(HostKeyRejection::Unknown)
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn accept_new_records_then_recognises() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir);
        let verifier = KnownHostsVerifier::new(&path, UnknownHostPolicy::AcceptNew);
        let k = key(KEY_A);
        assert_eq!(verifier.verify(&check("h", 2222, &k)).await, HostKeyDecision::Accept);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("[h]:2222 ssh-ed25519 "), "{text}");
        assert!(text.ends_with('\n'));

        let strict = KnownHostsVerifier::new(&path, UnknownHostPolicy::Reject);
        assert_eq!(strict.verify(&check("h", 2222, &k)).await, HostKeyDecision::Accept);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn appending_repairs_a_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("old.example {KEY_B}")).unwrap();
        let verifier = KnownHostsVerifier::new(&path, UnknownHostPolicy::AcceptNew);
        let k = key(KEY_A);
        verifier.verify(&check("h", 22, &k)).await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[tokio::test]
    async fn changed_key_is_rejected_even_with_accept_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("h {KEY_A}\n")).unwrap();
        let verifier = KnownHostsVerifier::new(&path, UnknownHostPolicy::AcceptNew);
        let k = key(KEY_B);
        match verifier.verify(&check("h", 22, &k)).await {
            HostKeyDecision::Reject(HostKeyRejection::Changed { known_fingerprint, location }) => {
                assert!(known_fingerprint.starts_with("SHA256:"));
                assert!(location.ends_with("known_hosts:1"), "{location}");
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    struct Fixed(HostKeyDecision);

    #[async_trait]
    impl HostKeyVerifier for Fixed {
        async fn verify(&self, _check: &HostKeyCheck<'_>) -> HostKeyDecision {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn ask_policy_records_only_when_approved() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir);
        let k = key(KEY_A);

        let declining = KnownHostsVerifier::new(
            &path,
            UnknownHostPolicy::Ask(Arc::new(Fixed(HostKeyDecision::Reject(
                HostKeyRejection::Declined,
            )))),
        );
        assert_eq!(
            declining.verify(&check("h", 22, &k)).await,
            HostKeyDecision::Reject(HostKeyRejection::Declined)
        );
        assert!(!path.exists());

        let approving = KnownHostsVerifier::new(
            &path,
            UnknownHostPolicy::Ask(Arc::new(Fixed(HostKeyDecision::Accept))),
        );
        assert_eq!(approving.verify(&check("h", 22, &k)).await, HostKeyDecision::Accept);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn unreadable_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be cannot be read as text.
        let verifier = KnownHostsVerifier::new(dir.path(), UnknownHostPolicy::AcceptNew);
        let k = key(KEY_A);
        assert!(matches!(
            verifier.verify(&check("h", 22, &k)).await,
            HostKeyDecision::Reject(HostKeyRejection::Failed(_))
        ));
    }
}
