//! Credential handling for the standalone backend.
//!
//! Design rules (see ../../../SECURITY.md):
//! - Secrets are read from the platform secret store on demand and kept in
//!   memory for the lifetime of a session snapshot.
//! - Secrets never appear in Debug/Display output, logs, or the wire protocol
//!   (the helper receives a key only inside the private stdio `session.open`
//!   frame, which never touches disk).
//! - No secret is ever written to the conversation, task history, workspace
//!   files, or the helper's session JSONL.

use std::collections::HashMap;
use std::fmt;

use zeroize::Zeroize;

use crate::provider::CredentialRef;

/// A secret string that redacts itself in Debug and zeroizes on drop.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Deliberately named so call sites are greppable.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(redacted)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("no secret stored for reference {0}")]
    NotFound(String),
    #[error("secret store unavailable: {0}")]
    Unavailable(String),
}

/// Read-only access to the platform secret store.
///
/// The Warp application layer implements this on top of `SecureStorage`
/// (macOS Keychain / Windows DPAPI / Linux Secret Service); tests use
/// [`InMemorySecretStore`].
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<SecretString, SecretError>;
}

/// Test/dev store. Never used by the packaged application.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    values: std::sync::Mutex<HashMap<String, String>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, key: impl Into<String>, value: impl Into<String>) {
        self.values
            .lock()
            .expect("secret store lock")
            .insert(key.into(), value.into());
    }

    pub fn remove(&self, key: &str) {
        self.values.lock().expect("secret store lock").remove(key);
    }
}

impl SecretStore for InMemorySecretStore {
    fn get(&self, key: &str) -> Result<SecretString, SecretError> {
        self.values
            .lock()
            .expect("secret store lock")
            .get(key)
            .map(|value| SecretString::new(value.clone()))
            .ok_or_else(|| SecretError::NotFound(key.to_string()))
    }
}

/// Resolve the credential for a profile at session-open time.
pub fn resolve_credential(
    store: &dyn SecretStore,
    reference: &CredentialRef,
) -> Result<Option<SecretString>, SecretError> {
    match reference {
        CredentialRef::None => Ok(None),
        CredentialRef::SecretStore { key } => store.get(key).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_in_debug_and_display() {
        let secret = SecretString::new("sk-super-secret");
        assert_eq!(format!("{secret:?}"), "SecretString(redacted)");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert_eq!(secret.expose_secret(), "sk-super-secret");
    }

    #[test]
    fn resolves_only_configured_references() {
        let store = InMemorySecretStore::new();
        store.insert("warpi/p1", "sk-1");
        assert!(resolve_credential(&store, &CredentialRef::None).unwrap().is_none());
        let key = resolve_credential(&store, &CredentialRef::SecretStore { key: "warpi/p1".into() })
            .unwrap()
            .unwrap();
        assert_eq!(key.expose_secret(), "sk-1");
        assert!(matches!(
            resolve_credential(&store, &CredentialRef::SecretStore { key: "missing".into() }),
            Err(SecretError::NotFound(_))
        ));
    }
}
