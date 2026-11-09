use std::fmt;

use zeroize::Zeroizing;

/// A password, passphrase or similar secret.
///
/// The memory is zeroed on drop, and `Debug` prints a placeholder rather than
/// the value. `russh` takes passwords as a plain `String` when it builds the
/// authentication packet, so a transient copy outside this type exists for the
/// duration of that call; that copy is not under this crate's control.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    /// Takes ownership of `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Returns the secret. Do not log or display the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_reveal_the_value() {
        let secret = SecretString::from("hunter2");
        assert_eq!(secret.expose(), "hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
    }
}
