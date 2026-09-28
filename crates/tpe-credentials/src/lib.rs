//! Credential storage for the text-processing-engine workbench.
//!
//! Secrets (API keys, library-proxy passwords, session cookies) are kept out of
//! code, logs, argv and plaintext databases. Three layers are provided:
//!
//! - [`CredentialStore`]: the storage trait (`service` + `account` -> [`Secret`]).
//! - Backends: [`KeychainStore`] (macOS login keychain; unavailable elsewhere),
//!   [`EncryptedFileStore`] (a single `XChaCha20-Poly1305` encrypted file keyed by an
//!   Argon2id passphrase derivation, all platforms) and [`MemoryStore`] (tests).
//! - Helpers: [`CookieJarStore`] (per-host cookies stored as one secret per host)
//!   and [`ApiKeys`] (well-known service names for the scholarly APIs and model
//!   providers the workbench talks to).

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

mod api_keys;
mod cookies;
mod file_store;
mod keychain;
mod memory;

use std::fmt;

use zeroize::Zeroize;

pub use api_keys::ApiKeys;
pub use cookies::{COOKIE_SERVICE, Cookie, CookieJarStore};
pub use file_store::{EncryptedFileStore, KdfParams};
pub use keychain::KeychainStore;
pub use memory::MemoryStore;

/// A secret value (API key, password, serialised cookies).
///
/// `Debug` prints `Secret(***)` and the backing bytes are overwritten with zeros
/// when the value is dropped. There is deliberately no `Display` impl.
pub struct Secret(pub String);

impl Secret {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the secret value. Do not log or print the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Errors from credential stores. Messages never contain secret values.
#[derive(Debug, thiserror::Error)]
pub enum CredError {
    /// The backend does not exist on this platform (e.g. the Keychain off macOS).
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
    /// Reading or writing the backing file failed.
    #[error("credential store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The stored data is not in the expected format.
    #[error("credential data is malformed: {0}")]
    Format(String),
    /// Authenticated decryption failed: wrong passphrase or a modified file.
    #[error("decryption failed: wrong passphrase or corrupted credential file")]
    Decrypt,
    /// Key derivation or encryption failed.
    #[error("cryptographic operation failed: {0}")]
    Crypto(String),
    /// The macOS Keychain returned an error status.
    #[error("keychain error {code}: {message}")]
    Keychain {
        /// `OSStatus` returned by Security.framework.
        code: i32,
        /// Human-readable message from Security.framework.
        message: String,
    },
    /// A caller-supplied argument was rejected (empty service or account, ...).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// A store of secrets addressed by `service` and `account`.
pub trait CredentialStore {
    /// Fetch a secret; `Ok(None)` when no entry exists.
    fn get(&self, service: &str, account: &str) -> Result<Option<Secret>, CredError>;
    /// Create or replace a secret.
    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), CredError>;
    /// Remove a secret; returns whether an entry existed.
    fn delete(&self, service: &str, account: &str) -> Result<bool, CredError>;
    /// List the accounts stored under `service`, sorted.
    fn list(&self, service: &str) -> Result<Vec<String>, CredError>;
}

impl<T: CredentialStore + ?Sized> CredentialStore for &T {
    fn get(&self, service: &str, account: &str) -> Result<Option<Secret>, CredError> {
        (**self).get(service, account)
    }

    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), CredError> {
        (**self).set(service, account, secret)
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredError> {
        (**self).delete(service, account)
    }

    fn list(&self, service: &str) -> Result<Vec<String>, CredError> {
        (**self).list(service)
    }
}

/// Reject empty service or account names (shared by every backend).
fn check_names(service: &str, account: &str) -> Result<(), CredError> {
    check_service(service)?;
    if account.trim().is_empty() {
        return Err(CredError::InvalidInput(
            "account must not be empty".to_owned(),
        ));
    }
    Ok(())
}

/// Reject an empty service name.
fn check_service(service: &str) -> Result<(), CredError> {
    if service.trim().is_empty() {
        return Err(CredError::InvalidInput(
            "service must not be empty".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CredError, CredentialStore, MemoryStore, Secret};

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(format!("{secret:#?}"), "Secret(***)");
        assert!(!format!("{:?}", Some(secret.clone())).contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn secret_constructors_agree() {
        let a = Secret::from("k");
        let b = Secret::from(String::from("k"));
        let c = Secret(String::from("k"));
        assert_eq!(a.expose(), b.expose());
        assert_eq!(b.expose(), c.expose());
    }

    #[test]
    fn empty_names_are_rejected() {
        let store = MemoryStore::new();
        let secret = Secret::new("x");
        assert!(matches!(
            store.set("", "acct", &secret),
            Err(CredError::InvalidInput(_))
        ));
        assert!(matches!(
            store.set("svc", " ", &secret),
            Err(CredError::InvalidInput(_))
        ));
    }

    #[test]
    fn store_works_through_a_reference() {
        let store = MemoryStore::new();
        let by_ref: &MemoryStore = &store;
        <&MemoryStore as CredentialStore>::set(&by_ref, "svc", "a", &Secret::new("1")).unwrap();
        let as_dyn: &dyn CredentialStore = &store;
        <&dyn CredentialStore as CredentialStore>::set(&as_dyn, "svc", "b", &Secret::new("2"))
            .unwrap();
        assert_eq!(
            store.list("svc").unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }
}
