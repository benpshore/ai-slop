//! The bearer token: 256 bits from the operating system's CSPRNG, made once
//! per state directory and kept in a file only its owner can read.
//!
//! The file protects the token from other user accounts on the machine. It
//! does not protect it from other processes running as the same user: they
//! can read the file, as they can read the PDFs themselves (docs/API.md,
//! "Threat model").

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use subtle::ConstantTimeEq as _;

/// The token file's name inside the state directory.
pub const TOKEN_FILE: &str = "api-token";

/// Random bytes in a token.
const TOKEN_BYTES: usize = 32;

/// The secret clients send as `Authorization: Bearer <token>`: 64 lower-case
/// hex digits. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The token text, for `--print-token` and for a client embedding the
    /// server in-process.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `presented` is this token, compared in constant time (the
    /// length, which is public, is compared first).
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.0.as_bytes().ct_eq(presented).into()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Why the token could not be read or made.
#[derive(Debug)]
pub enum TokenError {
    Io(PathBuf, io::Error),
    /// The operating system's random source failed.
    Random(String),
    /// The token path is a symbolic link; it is never followed.
    Symlink(PathBuf),
    NotAFile(PathBuf),
    /// Group or others have some access; the server will not use a token
    /// that other accounts may have read.
    Permissions(PathBuf, u32),
    /// Owned by another user.
    Owner(PathBuf),
    /// Not 64 lower-case hex digits.
    Malformed(PathBuf),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, error) => write!(f, "token file {}: {error}", path.display()),
            Self::Random(error) => write!(f, "the system random source failed: {error}"),
            Self::Symlink(path) => write!(
                f,
                "token file {} is a symbolic link; remove it and start again",
                path.display()
            ),
            Self::NotAFile(path) => write!(f, "token file {} is not a file", path.display()),
            Self::Permissions(path, mode) => write!(
                f,
                "token file {} has mode {mode:o}, readable beyond its owner; delete it to \
                 make a new token (it may have been read), or `chmod 600` it",
                path.display()
            ),
            Self::Owner(path) => write!(f, "token file {} belongs to another user", path.display()),
            Self::Malformed(path) => write!(
                f,
                "token file {} is damaged; delete it to make a new token",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TokenError {}

/// The token in `state_dir`, made (mode 0600) if there is none yet. The
/// directory is created (mode 0700) if missing; an existing directory's mode
/// is left alone.
///
/// Creation writes a hidden temporary file and hard-links it to
/// [`TOKEN_FILE`], which fails rather than replace a file, so two servers
/// starting at once agree on one token and nobody reads a half-written one.
pub fn load_or_create(state_dir: &Path) -> Result<Token, TokenError> {
    let path = state_dir.join(TOKEN_FILE);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .map_err(|e| TokenError::Io(state_dir.to_path_buf(), e))?;
    match read(&path) {
        Err(TokenError::Io(_, error)) if error.kind() == io::ErrorKind::NotFound => {}
        other => return other,
    }
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| TokenError::Random(e.to_string()))?;
    let mut text = hex::encode(bytes);
    text.push('\n');
    create(state_dir, &path, text.as_bytes())?;
    read(&path)
}

/// Publish `contents` at `path` without ever replacing an existing file.
fn create(state_dir: &Path, path: &Path, contents: &[u8]) -> Result<(), TokenError> {
    let io_error = |e| TokenError::Io(path.to_path_buf(), e);
    let mut unique = [0u8; 8];
    getrandom::fill(&mut unique).map_err(|e| TokenError::Random(e.to_string()))?;
    let temporary = state_dir.join(format!(
        ".{TOKEN_FILE}-{}-{}.partial",
        std::process::id(),
        hex::encode(unique)
    ));
    let staged = write_new(&temporary, contents).and_then(|()| fs::hard_link(&temporary, path));
    let _ = fs::remove_file(&temporary);
    match staged {
        // Another process made it first: use theirs.
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        // No hard links on this file system: create the name exclusively.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
            ) =>
        {
            match write_new(path, contents) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(io_error(error)),
            }
        }
        Err(error) => Err(io_error(error)),
    }
}

/// Create `path` (mode 0600, never through a symbolic link, never over an
/// existing file) and write all of `contents` to disk.
fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Read and check an existing token file.
fn read(path: &Path) -> Result<Token, TokenError> {
    let owned = || path.to_path_buf();
    let file: File = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(TokenError::Symlink(owned()));
        }
        Err(error) => return Err(TokenError::Io(owned(), error)),
    };
    let meta = file.metadata().map_err(|e| TokenError::Io(owned(), e))?;
    if !meta.is_file() {
        return Err(TokenError::NotAFile(owned()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TokenError::Permissions(owned(), mode));
    }
    if meta.uid() != effective_uid() {
        return Err(TokenError::Owner(owned()));
    }
    let mut text = String::new();
    file.take(256)
        .read_to_string(&mut text)
        .map_err(|_| TokenError::Malformed(owned()))?;
    let token = text.strip_suffix('\n').unwrap_or(&text);
    let well_formed = token.len() == TOKEN_BYTES * 2
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if well_formed {
        Ok(Token(token.to_string()))
    } else {
        Err(TokenError::Malformed(owned()))
    }
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory of ours and
    // cannot fail (POSIX.1-2017).
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::{TOKEN_FILE, TokenError, load_or_create};
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn made_once_with_mode_0600_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let first = load_or_create(&state).unwrap();
        assert_eq!(first.expose().len(), 64);
        let path = state.join(TOKEN_FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "a directory it creates is private");
        let second = load_or_create(&state).unwrap();
        assert_eq!(first, second, "the same token on the next start");
        assert_eq!(format!("{first:?}"), "Token(<redacted>)");
        let leftovers: Vec<_> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [TOKEN_FILE], "no temporary file is left");
    }

    #[test]
    fn two_tokens_differ() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            load_or_create(a.path()).unwrap(),
            load_or_create(b.path()).unwrap()
        );
    }

    #[test]
    fn a_file_others_can_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        load_or_create(dir.path()).unwrap();
        let path = dir.path().join(TOKEN_FILE);
        for mode in [0o640, 0o604, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(load_or_create(dir.path()), Err(TokenError::Permissions(_, m)) if m == mode),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn a_symlink_or_damaged_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "0".repeat(64)).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o600)).unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::os::unix::fs::symlink(&elsewhere, state.join(TOKEN_FILE)).unwrap();
        assert!(matches!(
            load_or_create(&state),
            Err(TokenError::Symlink(_))
        ));

        let damaged = dir.path().join("damaged");
        std::fs::create_dir(&damaged).unwrap();
        let path = damaged.join(TOKEN_FILE);
        std::fs::write(&path, "not a token\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_or_create(&damaged),
            Err(TokenError::Malformed(_))
        ));
    }

    #[test]
    fn matching_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let token = load_or_create(dir.path()).unwrap();
        let text = token.expose().to_string();
        assert!(token.matches(text.as_bytes()));
        assert!(!token.matches(text.to_uppercase().as_bytes()));
        assert!(!token.matches(&text.as_bytes()[..63]));
        assert!(!token.matches(format!("{text}0").as_bytes()));
        assert!(!token.matches(b""));
    }
}
