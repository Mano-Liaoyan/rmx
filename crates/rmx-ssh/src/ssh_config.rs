//! A parser for OpenSSH `ssh_config` files, for importing connections.
//!
//! Understands `Host` blocks with `*`, `?` and `!` patterns and the `HostName`,
//! `User`, `Port`, `IdentityFile` and `ProxyJump` directives, following
//! OpenSSH's rule that the first value found wins (`IdentityFile` accumulates).
//!
//! `Match`, `Include` and `ProxyCommand` affect how a host connects but are
//! not interpreted; they are listed in [`SshConfig::warnings`] so an importer
//! can tell the user. Other directives are ignored.
//!
//! ```
//! use rmx_ssh::ssh_config::SshConfig;
//!
//! let config = SshConfig::parse("Host web\n  HostName 10.0.0.5\n  Port 2222\n");
//! let host = config.resolve("web");
//! assert_eq!(host.host_name, "10.0.0.5");
//! assert_eq!(host.port, Some(2222));
//! ```

use std::path::{Path, PathBuf};

use crate::pattern::pattern_list_matches;

/// A parsed configuration file.
#[derive(Clone, Debug, Default)]
pub struct SshConfig {
    blocks: Vec<Block>,
    warnings: Vec<ConfigWarning>,
}

/// A line that could not be honoured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigWarning {
    /// One-based line number.
    pub line: usize,
    /// What is wrong with it.
    pub message: String,
}

#[derive(Clone, Debug, Default)]
struct Block {
    /// Empty for directives before the first `Host`, which apply to every host.
    patterns: Vec<String>,
    settings: Vec<Setting>,
}

#[derive(Clone, Debug)]
enum Setting {
    HostName(String),
    User(String),
    Port(u16),
    IdentityFile(String),
    /// Empty for `ProxyJump none`.
    ProxyJump(Vec<JumpSpec>),
}

/// One hop from a `ProxyJump` directive.
///
/// `host` may itself be an alias; pass it to [`SshConfig::resolve`] to get its
/// settings, as OpenSSH would.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpSpec {
    /// Host name or alias.
    pub host: String,
    /// User, if given.
    pub user: Option<String>,
    /// Port, if given.
    pub port: Option<u16>,
}

/// The effective settings for one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHost {
    /// The name that was looked up.
    pub alias: String,
    /// The address to connect to; the alias unless `HostName` says otherwise.
    pub host_name: String,
    /// `User`, if set. OpenSSH defaults to the local user.
    pub user: Option<String>,
    /// `Port`, if set. OpenSSH defaults to 22.
    pub port: Option<u16>,
    /// `IdentityFile` values in order, with `~` and tokens expanded.
    pub identity_files: Vec<PathBuf>,
    /// Jump hosts in connection order, empty if none.
    pub proxy_jump: Vec<JumpSpec>,
}

impl SshConfig {
    /// Parses configuration text.
    pub fn parse(text: &str) -> Self {
        let mut config = SshConfig { blocks: vec![Block::default()], warnings: Vec::new() };
        // Between a `Match` and the next `Host`, directives have an unknown
        // scope, so they must not be attributed to anything.
        let mut in_match = false;
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            let Some((keyword, arguments)) = split_directive(raw) else {
                continue;
            };
            match keyword.to_ascii_lowercase().as_str() {
                "host" => {
                    in_match = false;
                    if arguments.is_empty() {
                        config.warn(number, "Host with no patterns");
                    }
                    config.blocks.push(Block { patterns: arguments, settings: Vec::new() });
                },
                "match" => {
                    in_match = true;
                    config
                        .warn(number, "Match blocks are not supported; its directives are skipped");
                },
                _ if in_match => {},
                "include" => config.warn(number, "Include is not followed"),
                "proxycommand" => config.warn(number, "ProxyCommand is not supported"),
                lowered => config.apply(number, lowered, &arguments),
            }
        }
        config
    }

    /// Reads and parses the file at `path`.
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self::parse(&std::fs::read_to_string(path)?))
    }

    fn warn(&mut self, line: usize, message: impl Into<String>) {
        self.warnings.push(ConfigWarning { line, message: message.into() });
    }

    fn apply(&mut self, line: usize, keyword: &str, arguments: &[String]) {
        let Some(first) = arguments.first() else {
            if is_known(keyword) {
                self.warn(line, format!("{keyword} has no value"));
            }
            return;
        };
        let setting = match keyword {
            "hostname" => Setting::HostName(first.clone()),
            "user" => Setting::User(first.clone()),
            "identityfile" => Setting::IdentityFile(first.clone()),
            "port" => match first.parse() {
                Ok(port) => Setting::Port(port),
                Err(_) => return self.warn(line, format!("invalid port {first:?}")),
            },
            "proxyjump" => match parse_proxy_jump(first) {
                Ok(hops) => Setting::ProxyJump(hops),
                Err(message) => return self.warn(line, message),
            },
            _ => return,
        };
        if let Some(block) = self.blocks.last_mut() {
            block.settings.push(setting);
        }
    }

    /// Lines that could not be honoured.
    pub fn warnings(&self) -> &[ConfigWarning] {
        &self.warnings
    }

    /// Host names named literally in `Host` lines, in file order and without
    /// duplicates. These are the entries an import would offer.
    pub fn hosts(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for block in &self.blocks {
            for pattern in &block.patterns {
                let literal = !pattern.contains(['*', '?']) && !pattern.starts_with('!');
                if literal && !names.contains(pattern) {
                    names.push(pattern.clone());
                }
            }
        }
        names
    }

    /// The effective settings for `alias`, expanding `~` from the current
    /// user's home directory.
    pub fn resolve(&self, alias: &str) -> ResolvedHost {
        self.resolve_with_home(alias, std::env::home_dir().as_deref())
    }

    /// As [`resolve`](Self::resolve), with an explicit home directory.
    pub fn resolve_with_home(&self, alias: &str, home: Option<&Path>) -> ResolvedHost {
        let mut host_name = None;
        let mut user = None;
        let mut port = None;
        let mut identity_files = Vec::new();
        let mut proxy_jump = None;
        let matching = self.blocks.iter().filter(|block| {
            block.patterns.is_empty()
                || pattern_list_matches(block.patterns.iter().map(String::as_str), alias)
        });
        for setting in matching.flat_map(|block| &block.settings) {
            match setting {
                Setting::HostName(value) => {
                    host_name.get_or_insert_with(|| value.clone());
                },
                Setting::User(value) => {
                    user.get_or_insert_with(|| value.clone());
                },
                Setting::Port(value) => {
                    port.get_or_insert(*value);
                },
                Setting::IdentityFile(value) => identity_files.push(value.clone()),
                Setting::ProxyJump(hops) => {
                    proxy_jump.get_or_insert_with(|| hops.clone());
                },
            }
        }
        let host_name =
            host_name.map_or_else(|| alias.to_owned(), |name: String| name.replace("%h", alias));
        let expansion = Expansion { host: &host_name, user: user.as_deref(), port, home };
        ResolvedHost {
            alias: alias.to_owned(),
            identity_files: identity_files.iter().map(|path| expansion.path(path)).collect(),
            host_name: host_name.clone(),
            user,
            port,
            proxy_jump: proxy_jump.unwrap_or_default(),
        }
    }
}

fn is_known(keyword: &str) -> bool {
    matches!(keyword, "hostname" | "user" | "port" | "identityfile" | "proxyjump")
}

/// Splits a line into its keyword and arguments, or `None` for blanks and
/// comments. Keywords end at whitespace or `=`, and arguments may be quoted.
fn split_directive(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    Some((keyword.to_owned(), split_arguments(rest)))
}

fn split_arguments(text: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            break;
        };
        if first == '#' {
            break;
        }
        let mut argument = String::new();
        if first == '"' {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                argument.push(c);
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                argument.push(c);
            }
        }
        arguments.push(argument);
    }
    arguments
}

fn parse_proxy_jump(value: &str) -> Result<Vec<JumpSpec>, String> {
    if value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    value.split(',').map(parse_jump).collect()
}

fn parse_jump(spec: &str) -> Result<JumpSpec, String> {
    let bad = || format!("invalid ProxyJump hop {spec:?}");
    let body = spec.strip_prefix("ssh://").unwrap_or(spec);
    let (user, address) = match body.rsplit_once('@') {
        Some((user, address)) => (Some(user.to_owned()), address),
        None => (None, body),
    };
    let (host, port) = if let Some(bracketed) = address.strip_prefix('[') {
        let (host, rest) = bracketed.split_once(']').ok_or_else(bad)?;
        match rest {
            "" => (host, None),
            _ => (host, Some(rest.strip_prefix(':').ok_or_else(bad)?)),
        }
    } else {
        match address.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        }
    };
    if host.is_empty() {
        return Err(bad());
    }
    let port = port.map(|p| p.parse().map_err(|_| bad())).transpose()?;
    Ok(JumpSpec { host: host.to_owned(), user, port })
}

/// Expansion of `~` and `%` tokens in paths.
struct Expansion<'a> {
    host: &'a str,
    user: Option<&'a str>,
    port: Option<u16>,
    home: Option<&'a Path>,
}

impl Expansion<'_> {
    fn path(&self, value: &str) -> PathBuf {
        let expanded = self.tokens(value);
        if let Some(home) = self.home {
            if expanded == "~" {
                return home.to_path_buf();
            }
            if let Some(rest) = expanded.strip_prefix("~/") {
                return home.join(rest);
            }
        }
        PathBuf::from(expanded)
    }

    fn tokens(&self, value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('h') => out.push_str(self.host),
                Some('p') => out.push_str(&self.port.unwrap_or(22).to_string()),
                Some('r') if self.user.is_some() => out.push_str(self.user.unwrap_or_default()),
                Some('d') if self.home.is_some() => {
                    out.push_str(&self.home.unwrap_or(Path::new("")).to_string_lossy());
                },
                Some(other) => {
                    out.push('%');
                    out.push(other);
                },
                None => out.push('%'),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/test";

    fn resolve(config: &str, alias: &str) -> ResolvedHost {
        SshConfig::parse(config).resolve_with_home(alias, Some(Path::new(HOME)))
    }

    fn jump(host: &str, user: Option<&str>, port: Option<u16>) -> JumpSpec {
        JumpSpec { host: host.into(), user: user.map(Into::into), port }
    }

    #[test]
    fn unknown_alias_falls_back_to_itself() {
        let host = resolve("Host web\n  HostName 10.0.0.1\n", "other");
        assert_eq!(host.host_name, "other");
        assert_eq!(host.user, None);
        assert_eq!(host.port, None);
        assert!(host.identity_files.is_empty());
        assert!(host.proxy_jump.is_empty());
    }

    #[test]
    fn basic_block() {
        let config = "\
Host web
    HostName 10.0.0.1
    User deploy
    Port 2222
    IdentityFile ~/.ssh/deploy
    ProxyJump bastion
";
        let host = resolve(config, "web");
        assert_eq!(host.host_name, "10.0.0.1");
        assert_eq!(host.user.as_deref(), Some("deploy"));
        assert_eq!(host.port, Some(2222));
        assert_eq!(host.identity_files, [PathBuf::from("/home/test/.ssh/deploy")]);
        assert_eq!(host.proxy_jump, [jump("bastion", None, None)]);
    }

    #[test]
    fn keywords_are_case_insensitive_and_accept_equals() {
        let host = resolve("HOST a\nhostname=x.example\nUSER = bob\nport\t=\t2200\n", "a");
        assert_eq!(host.host_name, "x.example");
        assert_eq!(host.user.as_deref(), Some("bob"));
        assert_eq!(host.port, Some(2200));
    }

    #[test]
    fn comments_blank_lines_and_trailing_comments() {
        let host = resolve("# top\n\nHost a # trailing\n  User bob # who\n", "a");
        assert_eq!(host.user.as_deref(), Some("bob"));
        assert_eq!(SshConfig::parse("Host a # trailing\n").hosts(), ["a"]);
    }

    #[test]
    fn quoted_values_keep_spaces() {
        let host = resolve("Host a\n  IdentityFile \"~/my keys/id\"\n", "a");
        assert_eq!(host.identity_files, [PathBuf::from("/home/test/my keys/id")]);
    }

    #[test]
    fn first_value_wins_across_blocks() {
        let config = "\
Host web
  User first
Host *
  User second
  Port 2022
";
        let host = resolve(config, "web");
        assert_eq!(host.user.as_deref(), Some("first"));
        assert_eq!(host.port, Some(2022));
    }

    #[test]
    fn first_value_wins_within_a_block() {
        let host = resolve("Host a\n  User one\n  User two\n", "a");
        assert_eq!(host.user.as_deref(), Some("one"));
    }

    #[test]
    fn directives_before_any_host_apply_globally() {
        let host = resolve("User global\nHost a\n  User local\n", "a");
        assert_eq!(host.user.as_deref(), Some("global"));
    }

    #[test]
    fn identity_files_accumulate_in_order() {
        let config = "\
Host a
  IdentityFile ~/one
Host *
  IdentityFile ~/two
";
        let host = resolve(config, "a");
        assert_eq!(
            host.identity_files,
            [PathBuf::from("/home/test/one"), PathBuf::from("/home/test/two")]
        );
    }

    #[test]
    fn wildcard_and_question_mark_patterns() {
        let config = "Host *.prod web?\n  User ops\n";
        assert_eq!(resolve(config, "db.prod").user.as_deref(), Some("ops"));
        assert_eq!(resolve(config, "web1").user.as_deref(), Some("ops"));
        assert_eq!(resolve(config, "web12").user, None);
    }

    #[test]
    fn negated_patterns_exclude() {
        let config = "Host * !bastion\n  ProxyJump bastion\n";
        assert_eq!(resolve(config, "web").proxy_jump.len(), 1);
        assert!(resolve(config, "bastion").proxy_jump.is_empty());
    }

    #[test]
    fn multiple_patterns_on_one_line() {
        let config = "Host a b c\n  User shared\n";
        assert_eq!(resolve(config, "b").user.as_deref(), Some("shared"));
        assert_eq!(SshConfig::parse(config).hosts(), ["a", "b", "c"]);
    }

    #[test]
    fn hosts_lists_literals_only() {
        let config = "Host web *.prod !skip\nHost web db\nHost *\n";
        assert_eq!(SshConfig::parse(config).hosts(), ["web", "db"]);
    }

    #[test]
    fn hostname_token_is_expanded() {
        let host = resolve("Host *\n  HostName %h.corp.example\n", "web");
        assert_eq!(host.host_name, "web.corp.example");
    }

    #[test]
    fn identity_tokens_are_expanded() {
        let config =
            "Host a\n  HostName real\n  User bob\n  Port 2200\n  IdentityFile ~/k/%r@%h_%p%%\n";
        assert_eq!(
            resolve(config, "a").identity_files,
            [PathBuf::from("/home/test/k/bob@real_2200%")]
        );
    }

    #[test]
    fn home_directory_token() {
        assert_eq!(
            resolve("Host a\n  IdentityFile %d/key\n", "a").identity_files,
            [PathBuf::from("/home/test/key")]
        );
    }

    #[test]
    fn unknown_tokens_are_kept() {
        assert_eq!(
            resolve("Host a\n  IdentityFile /x/%C\n", "a").identity_files,
            [PathBuf::from("/x/%C")]
        );
    }

    #[test]
    fn proxy_jump_forms() {
        let config = "Host a\n  ProxyJump alice@b1:2222,b2,[::1]:2200,ssh://carol@b3:22\n";
        assert_eq!(
            resolve(config, "a").proxy_jump,
            [
                jump("b1", Some("alice"), Some(2222)),
                jump("b2", None, None),
                jump("::1", None, Some(2200)),
                jump("b3", Some("carol"), Some(22)),
            ]
        );
    }

    #[test]
    fn proxy_jump_bare_ipv6() {
        assert_eq!(
            resolve("Host a\n  ProxyJump [fe80::1]\n", "a").proxy_jump,
            [jump("fe80::1", None, None)]
        );
    }

    #[test]
    fn proxy_jump_none_stops_later_values() {
        let config = "Host a\n  ProxyJump none\nHost *\n  ProxyJump bastion\n";
        assert!(resolve(config, "a").proxy_jump.is_empty());
        assert_eq!(resolve(config, "b").proxy_jump.len(), 1);
    }

    #[test]
    fn invalid_values_are_reported_not_applied() {
        let config = SshConfig::parse("Host a\n  Port banana\n  ProxyJump x:notaport\n  User\n");
        let lines: Vec<usize> = config.warnings().iter().map(|w| w.line).collect();
        assert_eq!(lines, [2, 3, 4]);
        let host = config.resolve_with_home("a", None);
        assert_eq!(host.port, None);
        assert!(host.proxy_jump.is_empty());
    }

    #[test]
    fn unsupported_directives_warn() {
        let config = SshConfig::parse(
            "Include foo\nProxyCommand nc %h %p\nMatch host x\n  User hidden\nHost a\n  User shown\n",
        );
        assert_eq!(config.warnings().len(), 3);
        assert_eq!(config.warnings()[2].line, 3);
        // Directives under Match must not leak into anything.
        let host = config.resolve_with_home("x", None);
        assert_eq!(host.user, None);
        assert_eq!(config.resolve_with_home("a", None).user.as_deref(), Some("shown"));
    }

    #[test]
    fn other_directives_are_ignored_silently() {
        let config = SshConfig::parse("Host a\n  ServerAliveInterval 30\n  ForwardAgent yes\n");
        assert!(config.warnings().is_empty());
    }

    #[test]
    fn load_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "Host a\n  User bob\n").unwrap();
        assert_eq!(SshConfig::load(&path).unwrap().resolve("a").user.as_deref(), Some("bob"));
        assert!(SshConfig::load(dir.path().join("missing")).is_err());
    }
}
