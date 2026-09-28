//! Where the Ask panel gets API keys from.
//!
//! The GUI only depends on the small [`KeyProvider`] trait. Today the sole
//! implementation reads environment variables ([`EnvKeyProvider`]); the
//! `tpe-credentials` crate (`CredentialStore`, macOS Keychain, encrypted file)
//! is developed on another branch, and the integration owner adds the adapter
//! `impl KeyProvider for <CredentialStore>` once both crates are on `main`.
//! The service names in [`services`] mirror that crate's `ApiKeys` constants so
//! the adapter is a one-line `store.get(service, account)`.
//!
//! Keys are never logged, never placed in argv and never written to the ledger.

use std::collections::HashMap;

use crate::tpe_ai::Provider;

/// Well-known credential service names (mirror of `tpe_credentials::ApiKeys`).
pub mod services {
    /// Anthropic API key.
    pub const ANTHROPIC: &str = "anthropic";
    /// `OpenAI` API key.
    pub const OPENAI: &str = "openai";
}

/// Source of API keys for the Ask panel.
pub trait KeyProvider {
    /// The key stored for `service` (one of [`services`]), or `None`.
    fn api_key(&self, service: &str) -> Option<String>;
}

/// Reads `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` from the process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvKeyProvider;

impl EnvKeyProvider {
    /// Environment variable holding the key for `service`, if the service is known.
    pub fn env_var(service: &str) -> Option<&'static str> {
        match service {
            services::ANTHROPIC => Some("ANTHROPIC_API_KEY"),
            services::OPENAI => Some("OPENAI_API_KEY"),
            _ => None,
        }
    }
}

impl KeyProvider for EnvKeyProvider {
    fn api_key(&self, service: &str) -> Option<String> {
        let name = Self::env_var(service)?;
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
}

/// Fixed in-memory keys, for tests and for wiring a key programmatically.
#[derive(Clone, Debug, Default)]
pub struct MapKeyProvider {
    keys: HashMap<String, String>,
}

impl MapKeyProvider {
    /// Adds or replaces the key for `service`.
    pub fn insert(&mut self, service: &str, key: &str) {
        self.keys.insert(service.to_owned(), key.to_owned());
    }
}

impl KeyProvider for MapKeyProvider {
    fn api_key(&self, service: &str) -> Option<String> {
        self.keys.get(service).cloned()
    }
}

/// Message shown when `provider` has no key. It names the service and the
/// environment variable without revealing anything secret.
pub fn missing_key_message(provider: Provider) -> String {
    let service = provider.credential_service();
    match EnvKeyProvider::env_var(service) {
        Some(var) => format!(
            "No API key for {} (service \"{service}\"). Set {var} before launching.",
            provider.label()
        ),
        None => format!(
            "No API key for {} (service \"{service}\").",
            provider.label()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_map_to_environment_variables() {
        assert_eq!(
            EnvKeyProvider::env_var(services::ANTHROPIC),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(
            EnvKeyProvider::env_var(services::OPENAI),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(EnvKeyProvider::env_var("nope"), None);
        assert_eq!(EnvKeyProvider.api_key("nope"), None);
    }

    #[test]
    fn map_provider_round_trips() {
        let mut keys = MapKeyProvider::default();
        assert_eq!(keys.api_key(services::ANTHROPIC), None);
        keys.insert(services::ANTHROPIC, "sk-a");
        assert_eq!(keys.api_key(services::ANTHROPIC).as_deref(), Some("sk-a"));
        assert_eq!(keys.api_key(services::OPENAI), None);
        let dynamic: &dyn KeyProvider = &keys;
        assert_eq!(
            dynamic.api_key(services::ANTHROPIC).as_deref(),
            Some("sk-a")
        );
    }

    #[test]
    fn missing_key_message_names_service_and_variable_only() {
        let message = missing_key_message(Provider::Anthropic);
        assert!(message.contains("Claude"));
        assert!(message.contains("\"anthropic\""));
        assert!(message.contains("ANTHROPIC_API_KEY"));
        let message = missing_key_message(Provider::OpenAI);
        assert!(message.contains("ChatGPT"));
        assert!(message.contains("OPENAI_API_KEY"));
    }
}
