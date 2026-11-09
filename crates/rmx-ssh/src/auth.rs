use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{Handle, Handler, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{self, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{AgentAuthError, MethodSet};
use zeroize::Zeroizing;

use crate::client::HostConfig;
use crate::error::{AuthAttempt, AuthFailure, Error};
use crate::secret::SecretString;

/// One way to authenticate. [`HostConfig::auth`] lists them in the order to try.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AuthMethod {
    /// Password authentication. With `None`, the [`Prompter`] is asked.
    Password(Option<SecretString>),
    /// A private key file, in any format OpenSSH writes. If the key is
    /// encrypted and `passphrase` is `None` or wrong, the [`Prompter`] is asked.
    PrivateKeyFile {
        /// Path to the private key.
        path: PathBuf,
        /// Passphrase for an encrypted key.
        passphrase: Option<SecretString>,
    },
    /// Every identity held by an ssh-agent.
    Agent {
        /// The agent's socket. With `None`, `SSH_AUTH_SOCK` is used.
        socket: Option<PathBuf>,
    },
    /// Keyboard-interactive authentication, answered by the [`Prompter`].
    KeyboardInteractive {
        /// Submethods to hint at to the server.
        submethods: Option<String>,
    },
}

impl AuthMethod {
    /// The method's name in the SSH protocol.
    fn wire_name(&self) -> &'static str {
        match self {
            AuthMethod::Password(_) => "password",
            AuthMethod::PrivateKeyFile { .. } | AuthMethod::Agent { .. } => "publickey",
            AuthMethod::KeyboardInteractive { .. } => "keyboard-interactive",
        }
    }

    /// The name reported in [`AuthAttempt::method`].
    fn label(&self) -> &'static str {
        match self {
            AuthMethod::Agent { .. } => "agent",
            other => other.wire_name(),
        }
    }
}

/// Which host and user a prompt is about.
#[derive(Clone, Copy, Debug)]
pub struct PromptContext<'a> {
    /// The host being authenticated to.
    pub host: &'a str,
    /// The port.
    pub port: u16,
    /// The user being authenticated.
    pub user: &'a str,
    /// Which try this is for the current method, starting at 1.
    pub attempt: u32,
}

/// One field of a keyboard-interactive round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardPrompt {
    /// The server's prompt text.
    pub text: String,
    /// Whether the answer may be shown as it is typed.
    pub echo: bool,
}

/// One keyboard-interactive round. A server can send several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardChallenge {
    /// The server's name for this round; may be empty.
    pub name: String,
    /// Instructions to show; may be empty.
    pub instructions: String,
    /// The fields to answer, in order. Never empty.
    pub prompts: Vec<KeyboardPrompt>,
}

/// Supplies authentication answers, normally from the UI or a credential store.
///
/// Prompts may wait on a user for as long as needed; connection timeouts do not
/// run meanwhile. Returning `None` cancels authentication, and [`connect`]
/// fails with [`Error::AuthCancelled`].
///
/// [`connect`]: crate::connect
#[async_trait]
pub trait Prompter: Send + Sync {
    /// Asks for the password.
    async fn password(&self, context: &PromptContext<'_>) -> Option<SecretString>;

    /// Asks for the passphrase of the key at `path`.
    async fn key_passphrase(
        &self,
        context: &PromptContext<'_>,
        path: &Path,
    ) -> Option<SecretString>;

    /// Answers a keyboard-interactive round: one response per prompt.
    async fn keyboard_interactive(
        &self,
        context: &PromptContext<'_>,
        challenge: &KeyboardChallenge,
    ) -> Option<Vec<SecretString>>;

    /// Shows the server's pre-authentication banner. The connection is held up
    /// until this returns, so return promptly. The default ignores it.
    async fn banner(&self, _host: &str, _text: &str) {}
}

/// A [`Prompter`] that cancels every prompt, for non-interactive use.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPrompter;

#[async_trait]
impl Prompter for NoPrompter {
    async fn password(&self, _context: &PromptContext<'_>) -> Option<SecretString> {
        None
    }

    async fn key_passphrase(
        &self,
        _context: &PromptContext<'_>,
        _path: &Path,
    ) -> Option<SecretString> {
        None
    }

    async fn keyboard_interactive(
        &self,
        _context: &PromptContext<'_>,
        _challenge: &KeyboardChallenge,
    ) -> Option<Vec<SecretString>> {
        None
    }
}

/// The result of trying one method.
enum Outcome {
    Authenticated,
    /// Move on to the next method; `methods` is what the server would accept.
    Failed {
        methods: Option<Vec<String>>,
        detail: String,
    },
}

fn method_names(methods: &MethodSet) -> Vec<String> {
    methods.iter().map(String::from).collect()
}

/// The authentication loop, tying a [`HostConfig`] to one russh [`Handle`].
pub(crate) struct Authenticator<'a, H: Handler> {
    pub handle: &'a mut Handle<H>,
    pub host: &'a HostConfig,
    pub prompter: &'a Arc<dyn Prompter>,
    pub timeout: Duration,
    pub prompt_attempts: u32,
}

impl<H: Handler> Authenticator<'_, H> {
    /// Tries each configured method in order until one succeeds.
    pub async fn run(mut self) -> Result<(), Error> {
        if self.host.auth.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "no authentication methods configured for {}",
                self.host.host
            )));
        }
        let mut attempts = Vec::new();
        let mut server_methods: Option<Vec<String>> = None;
        for method in self.host.auth.clone() {
            if server_methods.as_ref().is_some_and(|m| !m.iter().any(|n| n == method.wire_name())) {
                attempts.push(AuthAttempt {
                    method: method.label(),
                    detail: String::from("not offered by the server"),
                });
                continue;
            }
            match self.try_method(&method).await? {
                Outcome::Authenticated => return Ok(()),
                Outcome::Failed { methods, detail } => {
                    attempts.push(AuthAttempt { method: method.label(), detail });
                    if methods.is_some() {
                        server_methods = methods;
                    }
                },
            }
        }
        Err(Error::AuthFailed(AuthFailure {
            host: self.host.host.clone(),
            user: self.host.user.clone(),
            attempts,
            server_methods: server_methods.unwrap_or_default(),
        }))
    }

    async fn try_method(&mut self, method: &AuthMethod) -> Result<Outcome, Error> {
        match method {
            AuthMethod::Password(password) => self.password(password.as_ref()).await,
            AuthMethod::PrivateKeyFile { path, passphrase } => {
                self.private_key(path, passphrase.as_ref()).await
            },
            AuthMethod::Agent { socket } => self.agent(socket.as_deref()).await,
            AuthMethod::KeyboardInteractive { submethods } => {
                self.keyboard_interactive(submethods.clone()).await
            },
        }
    }

    fn context(&self, attempt: u32) -> PromptContext<'_> {
        PromptContext {
            host: &self.host.host,
            port: self.host.port,
            user: &self.host.user,
            attempt,
        }
    }

    fn cancelled(&self) -> Error {
        Error::AuthCancelled { host: self.host.host.clone(), user: self.host.user.clone() }
    }

    async fn bounded<T, E>(
        &self,
        stage: &'static str,
        step: impl Future<Output = Result<T, E>>,
    ) -> Result<T, Error>
    where
        Error: From<E>,
    {
        match tokio::time::timeout(self.timeout, step).await {
            Ok(result) => Ok(result?),
            Err(_) => {
                Err(Error::Timeout { host: self.host.host.clone(), port: self.host.port, stage })
            },
        }
    }

    /// Turns a failed attempt into an [`Outcome`], or an error if the session
    /// died: russh reports a dead session as an ordinary auth failure.
    fn failed(&self, methods: MethodSet, partial: bool, detail: &str) -> Result<Outcome, Error> {
        if self.handle.is_closed() {
            return Err(Error::Disconnected(crate::DisconnectReason::ConnectionClosed));
        }
        let detail = if partial {
            format!("{detail} (partial success, more needed)")
        } else {
            String::from(detail)
        };
        Ok(Outcome::Failed { methods: Some(method_names(&methods)), detail })
    }

    async fn password(&mut self, supplied: Option<&SecretString>) -> Result<Outcome, Error> {
        if let Some(password) = supplied {
            return self.send_password(password).await;
        }
        let mut outcome = Outcome::Failed { methods: None, detail: String::from("not attempted") };
        for attempt in 1..=self.prompt_attempts {
            let context = self.context(attempt);
            let Some(password) = self.prompter.password(&context).await else {
                return Err(self.cancelled());
            };
            outcome = self.send_password(&password).await?;
            if matches!(outcome, Outcome::Authenticated) {
                break;
            }
        }
        Ok(outcome)
    }

    async fn send_password(&mut self, password: &SecretString) -> Result<Outcome, Error> {
        let user = self.host.user.clone();
        let step = self.handle.authenticate_password(user, password.expose());
        let result = match tokio::time::timeout(self.timeout, step).await {
            Ok(result) => result?,
            Err(_) => return Err(self.step_timeout("password authentication")),
        };
        match result {
            russh::client::AuthResult::Success => Ok(Outcome::Authenticated),
            russh::client::AuthResult::Failure { remaining_methods, partial_success } => {
                self.failed(remaining_methods, partial_success, "rejected")
            },
        }
    }

    fn step_timeout(&self, stage: &'static str) -> Error {
        Error::Timeout { host: self.host.host.clone(), port: self.host.port, stage }
    }

    async fn private_key(
        &mut self,
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<Outcome, Error> {
        let key = match self.load_key(path, passphrase).await? {
            Ok(key) => key,
            Err(detail) => return Ok(Outcome::Failed { methods: None, detail }),
        };
        let hash = self.rsa_hash(key.algorithm().is_rsa()).await?;
        let user = self.host.user.clone();
        let credential = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        let step = self.handle.authenticate_publickey(user, credential);
        let result = match tokio::time::timeout(self.timeout, step).await {
            Ok(result) => result?,
            Err(_) => return Err(self.step_timeout("public key authentication")),
        };
        match result {
            russh::client::AuthResult::Success => Ok(Outcome::Authenticated),
            russh::client::AuthResult::Failure { remaining_methods, partial_success } => {
                self.failed(remaining_methods, partial_success, "key rejected")
            },
        }
    }

    /// Loads and decrypts a key. The inner `Err` is a problem with this key
    /// that should not stop later methods.
    async fn load_key(
        &self,
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<Result<PrivateKey, String>, Error> {
        let owned = path.to_owned();
        let text =
            match run_blocking(move || std::fs::read_to_string(owned).map(Zeroizing::new)).await {
                Ok(text) => text,
                Err(error) => return Ok(Err(format!("{}: {error}", path.display()))),
            };
        let text = Arc::new(text);

        let mut supplied = passphrase.cloned();
        let mut attempt = 0;
        loop {
            let text = Arc::clone(&text);
            let phrase = supplied.clone();
            let decoded = run_blocking(move || {
                keys::decode_secret_key(&text, phrase.as_ref().map(SecretString::expose))
            })
            .await;
            match decoded {
                Ok(key) => return Ok(Ok(key)),
                Err(keys::Error::KeyIsEncrypted) => {},
                Err(error) if supplied.is_some() && is_wrong_passphrase(&error) => {
                    if attempt >= self.prompt_attempts {
                        return Ok(Err(format!("{}: wrong passphrase", path.display())));
                    }
                },
                Err(error) => return Ok(Err(format!("{}: {error}", path.display()))),
            }
            attempt += 1;
            if attempt > self.prompt_attempts {
                return Ok(Err(format!("{}: passphrase required", path.display())));
            }
            let context = self.context(attempt);
            match self.prompter.key_passphrase(&context, path).await {
                Some(phrase) => supplied = Some(phrase),
                None => return Err(self.cancelled()),
            }
        }
    }

    /// The hash algorithm to sign with, for an RSA key.
    async fn rsa_hash(&self, is_rsa: bool) -> Result<Option<HashAlg>, Error> {
        if !is_rsa {
            return Ok(None);
        }
        let best = self.bounded("key algorithm negotiation", self.handle.best_supported_rsa_hash());
        Ok(best.await?.flatten())
    }

    async fn agent(&mut self, socket: Option<&Path>) -> Result<Outcome, Error> {
        let client = match socket {
            Some(path) => AgentClient::connect_uds(path).await,
            None => AgentClient::connect_env().await,
        };
        let mut client = match client {
            Ok(client) => client,
            Err(error) => {
                let detail = format!("could not reach the agent: {error}");
                return Ok(Outcome::Failed { methods: None, detail });
            },
        };
        let identities = match client.request_identities().await {
            Ok(identities) => identities,
            Err(error) => {
                let detail = format!("could not list agent keys: {error}");
                return Ok(Outcome::Failed { methods: None, detail });
            },
        };
        if identities.is_empty() {
            return Ok(Outcome::Failed {
                methods: None,
                detail: String::from("agent holds no keys"),
            });
        }

        let mut methods = None;
        let mut detail = String::from("no agent key was accepted");
        for identity in identities {
            let user = self.host.user.clone();
            let result = match identity {
                AgentIdentity::PublicKey { key, .. } => {
                    let hash = self.rsa_hash(key.algorithm().is_rsa()).await?;
                    let step =
                        self.handle.authenticate_publickey_with(user, key, hash, &mut client);
                    tokio::time::timeout(self.timeout, step).await
                },
                AgentIdentity::Certificate { certificate, .. } => {
                    let hash = self.rsa_hash(certificate.algorithm().is_rsa()).await?;
                    let step = self.handle.authenticate_certificate_with(
                        user,
                        certificate,
                        hash,
                        &mut client,
                    );
                    tokio::time::timeout(self.timeout, step).await
                },
            };
            match result {
                Err(_) => return Err(self.step_timeout("agent authentication")),
                Ok(Ok(russh::client::AuthResult::Success)) => return Ok(Outcome::Authenticated),
                Ok(Ok(russh::client::AuthResult::Failure { remaining_methods, .. })) => {
                    if self.handle.is_closed() {
                        return Err(Error::Disconnected(crate::DisconnectReason::ConnectionClosed));
                    }
                    methods = Some(method_names(&remaining_methods));
                },
                // The agent refused this one key; another may still work.
                Ok(Err(AgentAuthError::Key(error))) => detail = format!("agent: {error}"),
                Ok(Err(AgentAuthError::Send(_))) => {
                    return Err(Error::Disconnected(crate::DisconnectReason::ConnectionClosed));
                },
            }
        }
        Ok(Outcome::Failed { methods, detail })
    }

    async fn keyboard_interactive(&mut self, submethods: Option<String>) -> Result<Outcome, Error> {
        let user = self.host.user.clone();
        let step = self.handle.authenticate_keyboard_interactive_start(user, submethods);
        let mut response = match tokio::time::timeout(self.timeout, step).await {
            Ok(result) => result?,
            Err(_) => return Err(self.step_timeout("keyboard-interactive authentication")),
        };
        let mut round = 0;
        loop {
            match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(Outcome::Authenticated),
                KeyboardInteractiveAuthResponse::Failure { remaining_methods, partial_success } => {
                    return self.failed(remaining_methods, partial_success, "rejected");
                },
                KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                    round += 1;
                    let answers = self.answer_round(round, name, instructions, prompts).await?;
                    let step = self.handle.authenticate_keyboard_interactive_respond(answers);
                    response = match tokio::time::timeout(self.timeout, step).await {
                        Ok(result) => result?,
                        Err(_) => {
                            return Err(self.step_timeout("keyboard-interactive authentication"));
                        },
                    };
                },
            }
        }
    }

    async fn answer_round(
        &self,
        round: u32,
        name: String,
        instructions: String,
        prompts: Vec<russh::client::Prompt>,
    ) -> Result<Vec<String>, Error> {
        // Servers send empty rounds as a final step; there is nothing to ask.
        if prompts.is_empty() {
            return Ok(Vec::new());
        }
        let expected = prompts.len();
        let challenge = KeyboardChallenge {
            name,
            instructions,
            prompts: prompts
                .into_iter()
                .map(|p| KeyboardPrompt { text: p.prompt, echo: p.echo })
                .collect(),
        };
        let context = self.context(round);
        let Some(answers) = self.prompter.keyboard_interactive(&context, &challenge).await else {
            return Err(self.cancelled());
        };
        if answers.len() != expected {
            return Err(Error::InvalidConfig(format!(
                "prompter answered {} of {expected} keyboard-interactive prompts",
                answers.len()
            )));
        }
        Ok(answers.iter().map(|a| a.expose().to_owned()).collect())
    }
}

/// A wrong passphrase decrypts to garbage, which `ssh-key` reports as a
/// decryption failure or a bad checksum.
fn is_wrong_passphrase(error: &keys::Error) -> bool {
    matches!(
        error,
        keys::Error::SshKey(_)
            | keys::Error::KeyIsCorrupt
            | keys::Error::CouldNotReadKey
            | keys::Error::Unpad(_)
    )
}

/// Runs CPU- or disk-bound work off the async executor.
async fn run_blocking<T, E>(work: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static + From<std::io::Error>,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(E::from(std::io::Error::other(error))),
    }
}
