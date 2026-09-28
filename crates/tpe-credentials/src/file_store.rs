//! A passphrase-protected credential file that works on every platform.
//!
//! File layout (all integers little-endian):
//!
//! | bytes | content |
//! |-------|---------|
//! | 8     | magic `TPECRED1` |
//! | 4 x 3 | Argon2id memory (kibibytes), iterations, parallelism |
//! | 24    | Argon2id salt |
//! | 24    | `XChaCha20-Poly1305` nonce (fresh for every write) |
//! | rest  | ciphertext of the JSON map `service -> account -> secret`, plus 16-byte tag |
//!
//! The 44-byte header (magic, KDF parameters, salt) is authenticated as
//! associated data, so it cannot be altered without decryption failing.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::array::Array;
use chacha20poly1305::aead::{Aead, Generate, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::{CredError, CredentialStore, Secret, check_names, check_service};

const MAGIC: &[u8; 8] = b"TPECRED1";
const SALT_LEN: usize = 24;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 8 + 3 * 4 + SALT_LEN;
const MAX_MEMORY_KIB: u32 = 1_048_576;
const MAX_ITERATIONS: u32 = 64;
const MAX_PARALLELISM: u32 = 16;

/// Argon2id cost parameters used when a new credential file is created.
/// Existing files always use the parameters stored in their header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory cost in kibibytes (default 19 * 1024).
    pub memory_kib: u32,
    /// Number of passes (default 2).
    pub iterations: u32,
    /// Degree of parallelism (default 1).
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_kib: Params::DEFAULT_M_COST,
            iterations: Params::DEFAULT_T_COST,
            parallelism: Params::DEFAULT_P_COST,
        }
    }
}

impl KdfParams {
    /// Validate against sane bounds and convert to `argon2::Params` (32-byte output).
    fn to_argon2(self) -> Result<Params, CredError> {
        if self.memory_kib > MAX_MEMORY_KIB
            || self.iterations > MAX_ITERATIONS
            || self.parallelism > MAX_PARALLELISM
        {
            return Err(CredError::Format(
                "Argon2 parameters exceed the supported maximum".to_owned(),
            ));
        }
        Params::new(
            self.memory_kib,
            self.iterations,
            self.parallelism,
            Some(KEY_LEN),
        )
        .map_err(|e| CredError::Crypto(format!("invalid Argon2 parameters: {e}")))
    }
}

/// The authenticated, unencrypted part of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Header {
    kdf: KdfParams,
    salt: [u8; SALT_LEN],
}

impl Header {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.kdf.memory_kib.to_le_bytes());
        out.extend_from_slice(&self.kdf.iterations.to_le_bytes());
        out.extend_from_slice(&self.kdf.parallelism.to_le_bytes());
        out.extend_from_slice(&self.salt);
        out
    }

    fn parse(bytes: &[u8]) -> Result<Self, CredError> {
        if bytes.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
            return Err(CredError::Format(
                "not a tpe credential file (bad magic)".to_owned(),
            ));
        }
        let kdf = KdfParams {
            memory_kib: read_u32(bytes, 8)?,
            iterations: read_u32(bytes, 12)?,
            parallelism: read_u32(bytes, 16)?,
        };
        let salt: [u8; SALT_LEN] = bytes
            .get(20..HEADER_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| CredError::Format("truncated header".to_owned()))?;
        Ok(Self { kdf, salt })
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, CredError> {
    let chunk: [u8; 4] = bytes
        .get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| CredError::Format("truncated header".to_owned()))?;
    Ok(u32::from_le_bytes(chunk))
}

/// Decrypted contents: `service -> account -> secret`. Zeroised on drop.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
struct Vault {
    entries: BTreeMap<String, BTreeMap<String, String>>,
}

impl Drop for Vault {
    fn drop(&mut self) {
        for accounts in self.entries.values_mut() {
            for value in accounts.values_mut() {
                value.zeroize();
            }
        }
    }
}

/// Credential store backed by one encrypted file (`XChaCha20-Poly1305`, key from
/// Argon2id over a caller-supplied passphrase). The file is created with mode
/// 0600 on unix and replaced atomically (write temp file, fsync, rename) on
/// every change. Every operation re-reads the file, so several handles or
/// processes see each other's writes; concurrent writers are last-writer-wins.
pub struct EncryptedFileStore {
    path: PathBuf,
    header: Header,
    cipher: XChaCha20Poly1305,
    lock: Mutex<()>,
}

impl fmt::Debug for EncryptedFileStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedFileStore")
            .field("path", &self.path)
            .field("kdf", &self.header.kdf)
            .finish_non_exhaustive()
    }
}

impl EncryptedFileStore {
    /// Open (or create, with default Argon2id costs) the credential file at `path`.
    /// Fails with [`CredError::Decrypt`] when the passphrase is wrong.
    pub fn open(path: impl Into<PathBuf>, passphrase: &Secret) -> Result<Self, CredError> {
        Self::open_with_params(path, passphrase, KdfParams::default())
    }

    /// Like [`EncryptedFileStore::open`], using `params` if the file has to be
    /// created. An existing file keeps the parameters recorded in its header.
    pub fn open_with_params(
        path: impl Into<PathBuf>,
        passphrase: &Secret,
        params: KdfParams,
    ) -> Result<Self, CredError> {
        if passphrase.expose().is_empty() {
            return Err(CredError::InvalidInput(
                "passphrase must not be empty".to_owned(),
            ));
        }
        let path = path.into();
        match fs::read(&path) {
            Ok(bytes) => {
                let header = Header::parse(&bytes)?;
                let cipher = derive_cipher(passphrase, &header)?;
                let store = Self {
                    path,
                    header,
                    cipher,
                    lock: Mutex::new(()),
                };
                // Verify the passphrase now rather than on first use.
                store.decrypt(&bytes)?;
                Ok(store)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let salt: [u8; SALT_LEN] = XNonce::generate().0;
                let header = Header { kdf: params, salt };
                let cipher = derive_cipher(passphrase, &header)?;
                let store = Self {
                    path,
                    header,
                    cipher,
                    lock: Mutex::new(()),
                };
                store.write_vault(&Vault::default())?;
                Ok(store)
            }
            Err(err) => Err(CredError::Io(err)),
        }
    }

    /// Path of the backing file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Argon2id parameters recorded in the file header.
    pub fn kdf_params(&self) -> KdfParams {
        self.header.kdf
    }

    fn read_vault(&self) -> Result<Vault, CredError> {
        let bytes = fs::read(&self.path)?;
        self.decrypt(&bytes)
    }

    fn decrypt(&self, bytes: &[u8]) -> Result<Vault, CredError> {
        let header = Header::parse(bytes)?;
        if header != self.header {
            return Err(CredError::Format(
                "credential file was re-created since it was opened".to_owned(),
            ));
        }
        let aad = bytes
            .get(..HEADER_LEN)
            .ok_or_else(|| CredError::Format("truncated header".to_owned()))?;
        let nonce_bytes: [u8; NONCE_LEN] = bytes
            .get(HEADER_LEN..HEADER_LEN + NONCE_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| CredError::Format("truncated nonce".to_owned()))?;
        let body = bytes
            .get(HEADER_LEN + NONCE_LEN..)
            .filter(|b| b.len() >= TAG_LEN)
            .ok_or_else(|| CredError::Format("truncated ciphertext".to_owned()))?;
        let nonce: XNonce = Array(nonce_bytes);
        let plaintext = Zeroizing::new(
            self.cipher
                .decrypt(&nonce, Payload { msg: body, aad })
                .map_err(|_| CredError::Decrypt)?,
        );
        serde_json::from_slice::<Vault>(&plaintext).map_err(|e| {
            CredError::Format(format!(
                "decrypted vault is not valid JSON (line {}, column {})",
                e.line(),
                e.column()
            ))
        })
    }

    fn write_vault(&self, vault: &Vault) -> Result<(), CredError> {
        let plaintext = Zeroizing::new(
            serde_json::to_vec(vault)
                .map_err(|e| CredError::Format(format!("cannot serialise vault: {e}")))?,
        );
        let header = self.header.to_bytes();
        let nonce = XNonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_slice(),
                    aad: &header,
                },
            )
            .map_err(|_| CredError::Crypto("encryption failed".to_owned()))?;
        let mut out = Vec::with_capacity(header.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&nonce.0);
        out.extend_from_slice(&ciphertext);
        atomic_write(&self.path, &out)
    }
}

impl CredentialStore for EncryptedFileStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<Secret>, CredError> {
        check_names(service, account)?;
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let vault = self.read_vault()?;
        Ok(vault
            .entries
            .get(service)
            .and_then(|accounts| accounts.get(account))
            .map(|value| Secret(value.clone())))
    }

    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), CredError> {
        check_names(service, account)?;
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut vault = self.read_vault()?;
        let accounts = vault.entries.entry(service.to_owned()).or_default();
        if let Some(mut old) = accounts.insert(account.to_owned(), secret.expose().to_owned()) {
            old.zeroize();
        }
        self.write_vault(&vault)
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredError> {
        check_names(service, account)?;
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut vault = self.read_vault()?;
        let Some(accounts) = vault.entries.get_mut(service) else {
            return Ok(false);
        };
        let Some(mut old) = accounts.remove(account) else {
            return Ok(false);
        };
        old.zeroize();
        if accounts.is_empty() {
            vault.entries.remove(service);
        }
        self.write_vault(&vault)?;
        Ok(true)
    }

    fn list(&self, service: &str) -> Result<Vec<String>, CredError> {
        check_service(service)?;
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let vault = self.read_vault()?;
        Ok(vault
            .entries
            .get(service)
            .map_or_else(Vec::new, |accounts| accounts.keys().cloned().collect()))
    }
}

/// Derive the file key with Argon2id and build the AEAD cipher.
fn derive_cipher(passphrase: &Secret, header: &Header) -> Result<XChaCha20Poly1305, CredError> {
    let params = header.kdf.to_argon2()?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(passphrase.expose().as_bytes(), &header.salt, &mut *key)
        .map_err(|e| CredError::Crypto(format!("Argon2id key derivation failed: {e}")))?;
    Ok(XChaCha20Poly1305::new(&Array(*key)))
}

/// Write `data` to a sibling temp file (mode 0600 on unix), fsync it, then rename
/// it over `path` so readers never observe a partial file.
fn atomic_write(path: &Path, data: &[u8]) -> Result<(), CredError> {
    let file_name = path.file_name().ok_or_else(|| {
        CredError::InvalidInput("credential file path has no file name".to_owned())
    })?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp_path = path.with_file_name(tmp_name);
    let result = write_private(&tmp_path, data).and_then(|()| fs::rename(&tmp_path, path));
    if let Err(err) = result {
        // Best effort: do not leave ciphertext fragments behind.
        let _ = fs::remove_file(&tmp_path);
        return Err(CredError::Io(err));
    }
    #[cfg(unix)]
    sync_parent(path)?;
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{EncryptedFileStore, KdfParams};
    use crate::{CredError, CredentialStore, Secret};

    /// Cheap Argon2id costs so tests stay fast; production uses the defaults.
    fn fast() -> KdfParams {
        KdfParams {
            memory_kib: 64,
            iterations: 1,
            parallelism: 1,
        }
    }

    fn pass(text: &str) -> Secret {
        Secret::new(text)
    }

    #[test]
    fn encrypted_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        {
            let store = EncryptedFileStore::open_with_params(&path, &pass("correct horse"), fast())
                .unwrap();
            store
                .set("tpe.openalex", "default", &Secret::new("sk-openalex-123"))
                .unwrap();
            store
                .set("tpe.zotero", "default", &Secret::new("zot-456"))
                .unwrap();
        }
        let reopened =
            EncryptedFileStore::open_with_params(&path, &pass("correct horse"), fast()).unwrap();
        assert_eq!(reopened.kdf_params(), fast());
        assert_eq!(
            reopened
                .get("tpe.openalex", "default")
                .unwrap()
                .unwrap()
                .expose(),
            "sk-openalex-123"
        );
        assert_eq!(
            reopened
                .get("tpe.zotero", "default")
                .unwrap()
                .unwrap()
                .expose(),
            "zot-456"
        );
        assert!(reopened.get("tpe.zotero", "other").unwrap().is_none());
        assert!(reopened.get("nope", "default").unwrap().is_none());
    }

    #[test]
    fn encrypted_file_has_no_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let store = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        store
            .set("svc-name-visible?", "acct", &Secret::new("hunter2-secret"))
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"TPECRED1"));
        for needle in [&b"hunter2-secret"[..], &b"svc-name-visible?"[..]] {
            assert!(!bytes.windows(needle.len()).any(|w| w == needle));
        }
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let store = EncryptedFileStore::open_with_params(&path, &pass("right"), fast()).unwrap();
        store.set("svc", "acct", &Secret::new("value")).unwrap();
        let err = EncryptedFileStore::open_with_params(&path, &pass("wrong"), fast()).unwrap_err();
        assert!(matches!(err, CredError::Decrypt), "got {err:?}");
    }

    #[test]
    fn empty_passphrase_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let err = EncryptedFileStore::open_with_params(&path, &pass(""), fast()).unwrap_err();
        assert!(matches!(err, CredError::InvalidInput(_)));
        assert!(!path.exists());
    }

    #[test]
    fn tampered_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let store = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        store.set("svc", "acct", &Secret::new("value")).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        if let Some(last) = bytes.last_mut() {
            *last ^= 0x01;
        }
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(store.get("svc", "acct"), Err(CredError::Decrypt)));
    }

    #[test]
    fn not_a_credential_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        fs::write(&path, b"{\"plain\": \"json\"}").unwrap();
        let err = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap_err();
        assert!(matches!(err, CredError::Format(_)), "got {err:?}");
    }

    #[test]
    fn delete_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let store = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        assert!(store.list("svc").unwrap().is_empty());
        store.set("svc", "bob", &Secret::new("1")).unwrap();
        store.set("svc", "alice", &Secret::new("2")).unwrap();
        store.set("svc", "alice", &Secret::new("3")).unwrap();
        store.set("other", "carol", &Secret::new("4")).unwrap();
        assert_eq!(
            store.list("svc").unwrap(),
            vec!["alice".to_owned(), "bob".to_owned()]
        );
        assert_eq!(store.get("svc", "alice").unwrap().unwrap().expose(), "3");
        assert!(store.delete("svc", "alice").unwrap());
        assert!(!store.delete("svc", "alice").unwrap());
        assert!(!store.delete("missing", "alice").unwrap());
        assert_eq!(store.list("svc").unwrap(), vec!["bob".to_owned()]);
        assert!(store.delete("svc", "bob").unwrap());
        assert!(store.list("svc").unwrap().is_empty());
        assert_eq!(store.list("other").unwrap(), vec!["carol".to_owned()]);
    }

    #[test]
    fn second_handle_sees_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let first = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        let second = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        first.set("svc", "acct", &Secret::new("shared")).unwrap();
        assert_eq!(
            second.get("svc", "acct").unwrap().unwrap().expose(),
            "shared"
        );
    }

    #[test]
    fn no_temp_files_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let store = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        store.set("svc", "acct", &Secret::new("v")).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["creds.bin".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.bin");
        let store = EncryptedFileStore::open_with_params(&path, &pass("pw"), fast()).unwrap();
        store.set("svc", "acct", &Secret::new("v")).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn default_params_match_argon2_defaults() {
        let params = KdfParams::default();
        assert_eq!(params.memory_kib, 19 * 1024);
        assert_eq!(params.iterations, 2);
        assert_eq!(params.parallelism, 1);
    }
}
