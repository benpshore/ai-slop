//! In-process credential store, for tests and for callers that must not persist.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::{CredError, CredentialStore, Secret, check_names, check_service};

/// A credential store held in memory only. Values are [`Secret`]s, so they are
/// zeroised when replaced, deleted, or when the store is dropped.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<BTreeMap<(String, String), Secret>>,
}

impl MemoryStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<Secret>, CredError> {
        check_names(service, account)?;
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(entries
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), CredError> {
        check_names(service, account)?;
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.insert((service.to_owned(), account.to_owned()), secret.clone());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredError> {
        check_names(service, account)?;
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = entries.remove(&(service.to_owned(), account.to_owned()));
        Ok(removed.is_some())
    }

    fn list(&self, service: &str) -> Result<Vec<String>, CredError> {
        check_service(service)?;
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(entries
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, account)| account.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryStore;
    use crate::{CredentialStore, Secret};

    #[test]
    fn memory_store_round_trip_delete_list() {
        let store = MemoryStore::new();
        store.set("svc", "b", &Secret::new("2")).unwrap();
        store.set("svc", "a", &Secret::new("1")).unwrap();
        store.set("other", "z", &Secret::new("9")).unwrap();
        assert_eq!(store.get("svc", "a").unwrap().unwrap().expose(), "1");
        assert!(store.get("svc", "missing").unwrap().is_none());
        assert_eq!(
            store.list("svc").unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(store.delete("svc", "a").unwrap());
        assert!(!store.delete("svc", "a").unwrap());
        assert_eq!(store.list("svc").unwrap(), vec!["b".to_owned()]);
    }
}
