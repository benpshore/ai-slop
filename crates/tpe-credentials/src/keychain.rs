//! macOS login-keychain backend (generic password items).
//!
//! On every other platform [`KeychainStore::new`] returns
//! [`CredError::Unavailable`], so callers can fall back to the
//! [`EncryptedFileStore`](crate::EncryptedFileStore).

use crate::{CredError, CredentialStore, Secret, check_names, check_service};

/// Credential store backed by the macOS Keychain (generic passwords keyed by
/// service and account). Construct with [`KeychainStore::new`].
#[derive(Debug)]
#[non_exhaustive]
pub struct KeychainStore;

impl KeychainStore {
    /// Use the default (login) keychain.
    #[cfg(target_os = "macos")]
    pub fn new() -> Result<Self, CredError> {
        Ok(Self)
    }

    /// The Keychain only exists on macOS: always [`CredError::Unavailable`] here.
    #[cfg(not(target_os = "macos"))]
    pub fn new() -> Result<Self, CredError> {
        Err(platform::unavailable())
    }
}

impl CredentialStore for KeychainStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<Secret>, CredError> {
        check_names(service, account)?;
        platform::get(service, account)
    }

    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), CredError> {
        check_names(service, account)?;
        platform::set(service, account, secret)
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredError> {
        check_names(service, account)?;
        platform::delete(service, account)
    }

    fn list(&self, service: &str) -> Result<Vec<String>, CredError> {
        check_service(service)?;
        platform::list(service)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use security_framework::base::Error as SecError;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };
    use zeroize::Zeroize;

    use crate::{CredError, Secret};

    /// `errSecItemNotFound` from `SecBase.h` (security-framework-sys is not a
    /// direct dependency, so the documented constant is repeated here).
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;

    fn keychain_error(err: SecError) -> CredError {
        CredError::Keychain {
            code: err.code(),
            message: err.to_string(),
        }
    }

    pub fn get(service: &str, account: &str) -> Result<Option<Secret>, CredError> {
        match get_generic_password(service, account) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Ok(Some(Secret(text))),
                Err(err) => {
                    let mut raw = err.into_bytes();
                    raw.zeroize();
                    Err(CredError::Format(
                        "keychain item is not valid UTF-8".to_owned(),
                    ))
                }
            },
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(keychain_error(err)),
        }
    }

    pub fn set(service: &str, account: &str, secret: &Secret) -> Result<(), CredError> {
        set_generic_password(service, account, secret.expose().as_bytes()).map_err(keychain_error)
    }

    pub fn delete(service: &str, account: &str) -> Result<bool, CredError> {
        match delete_generic_password(service, account) {
            Ok(()) => Ok(true),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
            Err(err) => Err(keychain_error(err)),
        }
    }

    pub fn list(service: &str) -> Result<Vec<String>, CredError> {
        let mut options = ItemSearchOptions::new();
        options
            .class(ItemClass::generic_password())
            .service(service)
            .load_attributes(true)
            .limit(Limit::All);
        let results = match options.search() {
            Ok(results) => results,
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(Vec::new()),
            Err(err) => return Err(keychain_error(err)),
        };
        let mut accounts: Vec<String> = results
            .iter()
            .filter_map(SearchResult::simplify_dict)
            .filter_map(|attrs| attrs.get("acct").cloned())
            .collect();
        accounts.sort();
        accounts.dedup();
        Ok(accounts)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use crate::{CredError, Secret};

    pub fn unavailable() -> CredError {
        CredError::Unavailable("the macOS Keychain is only available on macOS".to_owned())
    }

    pub fn get(_service: &str, _account: &str) -> Result<Option<Secret>, CredError> {
        Err(unavailable())
    }

    pub fn set(_service: &str, _account: &str, _secret: &Secret) -> Result<(), CredError> {
        Err(unavailable())
    }

    pub fn delete(_service: &str, _account: &str) -> Result<bool, CredError> {
        Err(unavailable())
    }

    pub fn list(_service: &str) -> Result<Vec<String>, CredError> {
        Err(unavailable())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn keychain_unavailable_off_macos() {
        let result = super::KeychainStore::new();
        assert!(matches!(result, Err(crate::CredError::Unavailable(_))));
    }

    /// Touches the real login keychain, so it only runs on macOS with
    /// `TPE_KEYCHAIN_TESTS=1`.
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_round_trip() {
        use crate::{CredentialStore, Secret};

        if std::env::var("TPE_KEYCHAIN_TESTS").as_deref() != Ok("1") {
            eprintln!("skipped: set TPE_KEYCHAIN_TESTS=1 to exercise the macOS Keychain");
            return;
        }
        let store = super::KeychainStore::new().unwrap();
        let service = "tpe-credentials.test";
        let account = format!("round-trip-{}", std::process::id());
        store
            .set(service, &account, &Secret::new("keychain-value"))
            .unwrap();
        assert_eq!(
            store.get(service, &account).unwrap().unwrap().expose(),
            "keychain-value"
        );
        assert!(store.list(service).unwrap().contains(&account));
        assert!(store.delete(service, &account).unwrap());
        assert!(!store.delete(service, &account).unwrap());
        assert!(store.get(service, &account).unwrap().is_none());
    }
}
