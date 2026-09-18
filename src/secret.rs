//! Secret text whose storage is zeroized on drop and whose diagnostics never show it.
//!
//! A `Secret` is created at the point a credential enters the process (environment,
//! credential store, prompt, file, or token response) so no plain `String` outlives it.
//! Copies made by third-party code, such as HTTP header buffers or the keyring backend,
//! are outside this guarantee.

use serde::Deserialize;
use std::fmt;
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(transparent)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The plaintext, for presenting to the API or the credential store.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl From<Zeroizing<String>> for Secret {
    fn from(value: Zeroizing<String>) -> Self {
        Self(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_never_contain_the_secret() {
        let secret = Secret::new("do-not-print".into());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose(), "do-not-print");
    }

    #[test]
    fn secrets_deserialize_as_plain_strings() {
        let secret: Secret = serde_json::from_str("\"token\"").unwrap();
        assert_eq!(secret.expose(), "token");
    }
}
