//! Well-known service names for API keys used across the workbench.

use crate::{CredError, CredentialStore, Secret};

/// Well-known credential service names and helpers to read/write the single
/// key stored for each (account [`ApiKeys::ACCOUNT`]).
///
/// Use these constants instead of string literals so every crate (bibliographic
/// clients, Zotero, AI panel, browser) reads the same entry.
#[derive(Debug, Clone, Copy)]
pub struct ApiKeys;

impl ApiKeys {
    /// `OpenAlex` API key (premium / higher rate limits).
    pub const OPENALEX: &'static str = "tpe.openalex";
    /// Contact e-mail sent to Crossref as `mailto` (polite pool); not a secret,
    /// but stored alongside the keys so it stays out of config files and argv.
    pub const CROSSREF_MAILTO: &'static str = "tpe.crossref.mailto";
    /// Semantic Scholar Graph API key (`x-api-key` header).
    pub const SEMANTIC_SCHOLAR: &'static str = "tpe.semantic-scholar";
    /// NCBI E-utilities / PMC `api_key`.
    pub const NCBI: &'static str = "tpe.ncbi";
    /// Europe PMC credentials.
    pub const EUROPE_PMC: &'static str = "tpe.europe-pmc";
    /// Zotero Web API key.
    pub const ZOTERO: &'static str = "tpe.zotero";
    /// Anthropic API key (`x-api-key`).
    pub const ANTHROPIC: &'static str = "tpe.anthropic";
    /// `OpenAI` API key (`Authorization: Bearer`).
    pub const OPENAI: &'static str = "tpe.openai";
    /// Library proxy / link-resolver login (e.g. `EZproxy` password).
    pub const LIBRARY_PROXY: &'static str = "tpe.library-proxy";

    /// Every well-known service name, in declaration order.
    pub const ALL: [&'static str; 9] = [
        Self::OPENALEX,
        Self::CROSSREF_MAILTO,
        Self::SEMANTIC_SCHOLAR,
        Self::NCBI,
        Self::EUROPE_PMC,
        Self::ZOTERO,
        Self::ANTHROPIC,
        Self::OPENAI,
        Self::LIBRARY_PROXY,
    ];

    /// Account name used for the single key stored under each service.
    pub const ACCOUNT: &'static str = "default";

    /// Read the key for `service` (one of the constants above).
    pub fn get(store: &dyn CredentialStore, service: &str) -> Result<Option<Secret>, CredError> {
        store.get(service, Self::ACCOUNT)
    }

    /// Store or replace the key for `service`.
    pub fn set(store: &dyn CredentialStore, service: &str, key: &Secret) -> Result<(), CredError> {
        store.set(service, Self::ACCOUNT, key)
    }

    /// Remove the key for `service`; returns whether one was stored.
    pub fn delete(store: &dyn CredentialStore, service: &str) -> Result<bool, CredError> {
        store.delete(service, Self::ACCOUNT)
    }

    /// The well-known services that currently have a key in `store`.
    pub fn configured(store: &dyn CredentialStore) -> Result<Vec<&'static str>, CredError> {
        let mut out = Vec::new();
        for service in Self::ALL {
            if store.get(service, Self::ACCOUNT)?.is_some() {
                out.push(service);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::ApiKeys;
    use crate::{MemoryStore, Secret};

    #[test]
    fn service_names_are_unique_and_namespaced() {
        let unique: BTreeSet<&str> = ApiKeys::ALL.iter().copied().collect();
        assert_eq!(unique.len(), ApiKeys::ALL.len());
        assert!(ApiKeys::ALL.iter().all(|name| name.starts_with("tpe.")));
    }

    #[test]
    fn api_key_round_trip() {
        let store = MemoryStore::new();
        assert!(ApiKeys::get(&store, ApiKeys::ANTHROPIC).unwrap().is_none());
        ApiKeys::set(&store, ApiKeys::ANTHROPIC, &Secret::new("sk-ant-test")).unwrap();
        ApiKeys::set(&store, ApiKeys::NCBI, &Secret::new("ncbi-test")).unwrap();
        assert_eq!(
            ApiKeys::get(&store, ApiKeys::ANTHROPIC)
                .unwrap()
                .unwrap()
                .expose(),
            "sk-ant-test"
        );
        assert_eq!(
            ApiKeys::configured(&store).unwrap(),
            vec![ApiKeys::NCBI, ApiKeys::ANTHROPIC]
        );
        assert!(ApiKeys::delete(&store, ApiKeys::NCBI).unwrap());
        assert_eq!(
            ApiKeys::configured(&store).unwrap(),
            vec![ApiKeys::ANTHROPIC]
        );
    }
}
