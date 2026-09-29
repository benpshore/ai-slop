//! Credential lookup for the Ask panel.
//!
//! Persistent stores implementing [`CredentialStore`] are the preferred source.
//! Standard provider environment variables are retained only as a compatibility
//! fallback (including optional `OLLAMA_API_KEY` for authenticated remote Ollama).

use std::collections::HashMap;

use crate::tpe_ai::Provider;
pub use tpe_credentials::ApiKeys;
use tpe_credentials::CredentialStore;

/// Source of API keys for the Ask panel.
pub trait KeyProvider {
    /// The key stored for a canonical [`ApiKeys`] service, or `None`.
    fn api_key(&self, service: &str) -> Option<String>;
}

/// Reads standard provider variables as a compatibility fallback.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvKeyProvider;

impl EnvKeyProvider {
    /// Environment variable holding the key for a canonical service.
    pub fn env_var(service: &str) -> Option<&'static str> {
        match service {
            ApiKeys::OPENAI => Some("OPENAI_API_KEY"),
            ApiKeys::ANTHROPIC => Some("ANTHROPIC_API_KEY"),
            ApiKeys::GEMINI => Some("GEMINI_API_KEY"),
            ApiKeys::OLLAMA => Some("OLLAMA_API_KEY"),
            _ => None,
        }
    }
}

impl KeyProvider for EnvKeyProvider {
    fn api_key(&self, service: &str) -> Option<String> {
        std::env::var(Self::env_var(service)?)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }
}

/// Preferred persistent credential source, with environment compatibility fallback.
pub struct StoredKeyProvider<S> {
    store: S,
    env: EnvKeyProvider,
}
impl<S> StoredKeyProvider<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            env: EnvKeyProvider,
        }
    }
}
impl<S: CredentialStore> KeyProvider for StoredKeyProvider<S> {
    fn api_key(&self, service: &str) -> Option<String> {
        ApiKeys::get(&self.store, service)
            .ok()
            .flatten()
            .map(|secret| secret.expose().to_owned())
            .or_else(|| self.env.api_key(service))
    }
}

/// Fixed in-memory keys, for tests and programmatic wiring.
#[derive(Clone, Debug, Default)]
pub struct MapKeyProvider {
    keys: HashMap<String, String>,
}
impl MapKeyProvider {
    pub fn insert(&mut self, service: &str, key: &str) {
        self.keys.insert(service.to_owned(), key.to_owned());
    }
}
impl KeyProvider for MapKeyProvider {
    fn api_key(&self, service: &str) -> Option<String> {
        self.keys.get(service).cloned()
    }
}

/// Message shown when a provider has no key.
pub fn missing_key_message(provider: Provider) -> String {
    let metadata = provider.metadata();
    if !metadata.authentication.required() {
        return format!(
            "{} needs no API key for a trusted local endpoint. Set optional OLLAMA_API_KEY for an authenticated remote deployment.",
            metadata.label
        );
    }
    let Some(var) = EnvKeyProvider::env_var(metadata.credential_service) else {
        return format!(
            "No API key for {} (service \"{}\"). Save it in the credential store.",
            metadata.label, metadata.credential_service
        );
    };
    format!(
        "No API key for {} (service \"{}\"). Save it in the credential store (preferred), or set {var} as a compatibility fallback.",
        metadata.label, metadata.credential_service
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpe_credentials::{MemoryStore, Secret};

    #[test]
    fn canonical_services_map_to_environment_fallbacks() {
        let expected = [
            (ApiKeys::OPENAI, "OPENAI_API_KEY"),
            (ApiKeys::ANTHROPIC, "ANTHROPIC_API_KEY"),
            (ApiKeys::GEMINI, "GEMINI_API_KEY"),
            (ApiKeys::OLLAMA, "OLLAMA_API_KEY"),
        ];
        for (service, variable) in expected {
            assert_eq!(EnvKeyProvider::env_var(service), Some(variable));
        }
        assert_eq!(EnvKeyProvider::env_var("nope"), None);
    }

    #[test]
    fn persistent_store_is_preferred() {
        let store = MemoryStore::new();
        ApiKeys::set(&store, ApiKeys::OPENAI, &Secret::new("stored")).unwrap();
        assert_eq!(
            StoredKeyProvider::new(store)
                .api_key(ApiKeys::OPENAI)
                .as_deref(),
            Some("stored")
        );
    }

    #[test]
    fn map_provider_round_trips() {
        let mut keys = MapKeyProvider::default();
        keys.insert(ApiKeys::ANTHROPIC, "sk-a");
        assert_eq!(keys.api_key(ApiKeys::ANTHROPIC).as_deref(), Some("sk-a"));
    }

    #[test]
    fn ollama_message_documents_optional_authentication() {
        let message = missing_key_message(Provider::Ollama);
        assert!(message.contains("trusted local endpoint"));
        assert!(message.contains("optional OLLAMA_API_KEY"));
    }
}
