//! Secret-preserving credential access for the Ask panel.
//!
//! [`preferred_provider`] uses the login Keychain on macOS. Callers that cannot
//! use it must explicitly call [`encrypted_vault`] and obtain the passphrase
//! interactively (or from an OS-protected facility) in their callback. The
//! passphrase is never accepted as an argument to the executable or stored in
//! application configuration. [`EnvKeyProvider`] is an explicit fallback only.

use std::collections::HashMap;
use std::path::Path;

use tpe_credentials::{ApiKeys, CredentialStore, EncryptedFileStore, KeychainStore};
pub use tpe_credentials::{CredError, Secret};

/// Canonical credential service names.
pub mod services {
    /// Anthropic API key service.
    pub const ANTHROPIC: &str = tpe_credentials::ApiKeys::ANTHROPIC;
    /// `OpenAI` API key service.
    pub const OPENAI: &str = tpe_credentials::ApiKeys::OPENAI;
}

/// Source of API keys for the Ask panel.
pub trait KeyProvider {
    /// The key stored for `service`, or `None`.
    fn api_key(&self, service: &str) -> Result<Option<Secret>, CredError>;
}

/// Concrete adapter from the application interface to any credential backend.
pub struct CredentialKeyProvider {
    store: Box<dyn CredentialStore>,
}

impl std::fmt::Debug for CredentialKeyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialKeyProvider")
            .finish_non_exhaustive()
    }
}

impl CredentialKeyProvider {
    /// Wrap a store. This is also the injection point for [`MemoryStore`](tpe_credentials::MemoryStore) in tests.
    pub fn new(store: impl CredentialStore + 'static) -> Self {
        Self {
            store: Box::new(store),
        }
    }

    /// Store a key. The password-style UI field supplying `key` should be
    /// cleared after this call; stored values are deliberately never returned.
    pub fn set(&self, service: &str, key: &Secret) -> Result<(), CredError> {
        ApiKeys::set(self.store.as_ref(), service, key)
    }

    /// Explicitly replace an existing key without reading it back into the UI.
    pub fn replace(&self, service: &str, replacement: &Secret) -> Result<(), CredError> {
        ApiKeys::set(self.store.as_ref(), service, replacement)
    }

    /// Delete a key, returning whether it was configured.
    pub fn delete(&self, service: &str) -> Result<bool, CredError> {
        ApiKeys::delete(self.store.as_ref(), service)
    }

    /// Return configuration status without exposing the stored value.
    pub fn is_configured(&self, service: &str) -> Result<bool, CredError> {
        Ok(ApiKeys::get(self.store.as_ref(), service)?.is_some())
    }
}

impl KeyProvider for CredentialKeyProvider {
    fn api_key(&self, service: &str) -> Result<Option<Secret>, CredError> {
        ApiKeys::get(self.store.as_ref(), service)
    }
}

/// Prefer the macOS login Keychain. On unsupported platforms (or if opening
/// the Keychain fails), callers can explicitly choose [`encrypted_vault`].
pub fn preferred_provider() -> Result<CredentialKeyProvider, CredError> {
    Ok(CredentialKeyProvider::new(KeychainStore::new()?))
}

/// Explicitly open/create an encrypted vault. `request_passphrase` must prompt
/// with a password-style field or consult an OS-protected secret mechanism.
/// Its returned [`Secret`] is zeroized after vault initialization.
pub fn encrypted_vault(
    path: impl AsRef<Path>,
    request_passphrase: impl FnOnce() -> Result<Secret, CredError>,
) -> Result<CredentialKeyProvider, CredError> {
    let passphrase = request_passphrase()?;
    let store = EncryptedFileStore::open(path.as_ref(), &passphrase)?;
    drop(passphrase);
    Ok(CredentialKeyProvider::new(store))
}

/// Opt-in fallback reading `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvKeyProvider;

impl EnvKeyProvider {
    pub fn env_var(service: &str) -> Option<&'static str> {
        match service {
            services::ANTHROPIC => Some("ANTHROPIC_API_KEY"),
            services::OPENAI => Some("OPENAI_API_KEY"),
            _ => None,
        }
    }
}

impl KeyProvider for EnvKeyProvider {
    fn api_key(&self, service: &str) -> Result<Option<Secret>, CredError> {
        let Some(name) = Self::env_var(service) else {
            return Ok(None);
        };
        Ok(std::env::var(name)
            .ok()
            .map(Secret::new)
            .filter(|value| !value.expose().trim().is_empty()))
    }
}

/// Fixed in-memory keys for application tests and programmatic wiring.
#[derive(Debug, Default)]
pub struct MapKeyProvider {
    keys: HashMap<String, Secret>,
}

impl MapKeyProvider {
    pub fn insert(&mut self, service: &str, key: Secret) {
        self.keys.insert(service.to_owned(), key);
    }
}

impl KeyProvider for MapKeyProvider {
    fn api_key(&self, service: &str) -> Result<Option<Secret>, CredError> {
        // Ownership is required by KeyProvider; both copies are zeroized on drop.
        Ok(self.keys.get(service).cloned())
    }
}

pub fn missing_key_message(provider: crate::tpe_ai::Provider) -> String {
    format!("No API key configured for {}.", provider.label())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpe_credentials::MemoryStore;

    const CANARY: &str = "canary-credential-must-not-leak";

    #[test]
    fn credential_adapter_manages_memory_store_without_leaking() {
        let provider = CredentialKeyProvider::new(MemoryStore::new());
        assert!(!provider.is_configured(services::ANTHROPIC).unwrap());
        provider
            .set(services::ANTHROPIC, &Secret::new(CANARY))
            .unwrap();
        assert!(provider.is_configured(services::ANTHROPIC).unwrap());
        assert_eq!(
            provider
                .api_key(services::ANTHROPIC)
                .unwrap()
                .unwrap()
                .expose(),
            CANARY
        );
        provider
            .replace(services::ANTHROPIC, &Secret::new("replacement"))
            .unwrap();
        assert_eq!(
            provider
                .api_key(services::ANTHROPIC)
                .unwrap()
                .unwrap()
                .expose(),
            "replacement"
        );
        assert!(provider.delete(services::ANTHROPIC).unwrap());
        assert!(!provider.is_configured(services::ANTHROPIC).unwrap());
        assert!(!format!("{provider:?}").contains(CANARY));
    }

    #[test]
    fn map_and_errors_have_redacted_debug_output() {
        let mut keys = MapKeyProvider::default();
        keys.insert(services::OPENAI, Secret::new(CANARY));
        assert!(!format!("{keys:?}").contains(CANARY));
        let provider = CredentialKeyProvider::new(MemoryStore::new());
        let error = provider.api_key("").unwrap_err();
        assert!(!format!("{error:?} {error}").contains(CANARY));
    }
}
