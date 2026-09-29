//! Immutable snapshot of an input file: its complete bytes, their content
//! hash and where they were observed. The file is stat'ed before and after
//! the read so a concurrent modification is reported instead of hashed.

use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::schema::{ContentHash, SourceObservation, sha256_hex};

/// Whether acquisition may materialize an on-demand cloud placeholder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionPolicy {
    /// Never trigger an implicit download while opening an input.
    #[default]
    LocalOnly,
    /// Permit the operating system to hydrate an on-demand input.
    AllowHydration,
}

/// Platform-independent interpretation of filesystem flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Materialization {
    Local,
    Dataless,
}

#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
const UF_DATALESS: u32 = 0x4000_0000;

#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
fn classify_flags(flags: u32) -> Materialization {
    if flags & UF_DATALESS == 0 {
        Materialization::Local
    } else {
        Materialization::Dataless
    }
}

#[cfg(target_os = "macos")]
fn materialization(meta: &fs::Metadata) -> Materialization {
    use std::os::macos::fs::MetadataExt;
    classify_flags(meta.st_flags())
}

#[cfg(not(target_os = "macos"))]
fn materialization(_meta: &fs::Metadata) -> Materialization {
    Materialization::Local
}

/// Why a file could not be snapshotted.
#[derive(Debug, Error)]
pub enum AcquireError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("file is {size} bytes, larger than the {max} byte limit")]
    TooLarge { size: u64, max: u64 },
    #[error("file is empty")]
    Empty,
    #[error("file changed while it was being read")]
    ChangedDuringRead,
    #[error("not a regular file")]
    NotAFile,
    #[error("cloud placeholder is not materialized (hydration was not requested)")]
    Dataless,
    #[error(
        "materialization deferred: {size} bytes would exceed the remaining batch budget of {remaining} bytes"
    )]
    MaterializationBudget { size: u64, remaining: u64 },
}

/// Batch-wide allowance for inputs that may need local materialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializationBudget {
    remaining: u64,
}

impl MaterializationBudget {
    pub const fn new(bytes: u64) -> Self {
        Self { remaining: bytes }
    }
    pub const fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Reserve one input's logical size before acquisition begins.
    pub fn reserve(&mut self, size: u64) -> Result<(), AcquireError> {
        if size > self.remaining {
            return Err(AcquireError::MaterializationBudget {
                size,
                remaining: self.remaining,
            });
        }
        self.remaining -= size;
        Ok(())
    }
}

/// Return the logical size without opening or reading file contents.
pub fn logical_size(path: &Path) -> Result<u64, AcquireError> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() {
        return Err(AcquireError::NotAFile);
    }
    Ok(meta.len())
}

/// Bytes currently available on the filesystem containing `path`.
#[cfg(unix)]
pub fn available_space(path: &Path) -> Result<u64, AcquireError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let probe = if path.exists() {
        path
    } else {
        path.parent().unwrap_or(Path::new("."))
    };
    let path = CString::new(probe.as_os_str().as_bytes()).map_err(|_| {
        AcquireError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains NUL",
        ))
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(AcquireError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: a successful `statvfs` initialized the output structure.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
pub fn available_space(_path: &Path) -> Result<u64, AcquireError> {
    Ok(u64::MAX)
}

/// The bytes of one file at one instant, plus their identity and origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub hash: ContentHash,
    pub source: SourceObservation,
}

/// The verified bytes of one file before they are hashed, so the caller can
/// hash them on another thread (see [`read_verified`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unhashed {
    pub bytes: Vec<u8>,
    pub source: SourceObservation,
}

impl Unhashed {
    /// Attach the content hash computed from `self.bytes`.
    pub fn into_snapshot(self, hash: ContentHash) -> Snapshot {
        Snapshot {
            bytes: self.bytes,
            hash,
            source: self.source,
        }
    }
}

/// Fields of a `stat` call that must not change between the two calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Observed {
    size: u64,
    inode: Option<u64>,
    device: Option<u64>,
    mtime_unix: Option<i64>,
    /// Sub-second part of the modification time, so an in-place rewrite that
    /// starts and ends within one second is still detected.
    mtime_nsec: Option<i64>,
}

#[cfg(unix)]
fn observe(meta: &fs::Metadata) -> Observed {
    use std::os::unix::fs::MetadataExt;

    Observed {
        size: meta.len(),
        inode: Some(meta.ino()),
        device: Some(meta.dev()),
        mtime_unix: Some(meta.mtime()),
        mtime_nsec: Some(meta.mtime_nsec()),
    }
}

#[cfg(not(unix))]
fn unix_seconds(time: std::time::SystemTime) -> Option<i64> {
    use std::time::UNIX_EPOCH;

    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok(),
        Err(before) => {
            let secs = i64::try_from(before.duration().as_secs()).ok();
            secs.map(|value| -value)
        }
    }
}

#[cfg(not(unix))]
fn observe(meta: &fs::Metadata) -> Observed {
    Observed {
        size: meta.len(),
        inode: None,
        device: None,
        mtime_unix: meta.modified().ok().and_then(unix_seconds),
        mtime_nsec: None,
    }
}

/// Read `path` completely and hash it.
///
/// The file is stat'ed before and after the read; a size or mtime mismatch
/// (or a byte count that differs from the size) is reported as
/// [`AcquireError::ChangedDuringRead`]. `max_bytes` bounds the size accepted
/// before anything is read.
pub fn snapshot(path: &Path, max_bytes: Option<u64>) -> Result<Snapshot, AcquireError> {
    snapshot_with_policy(path, max_bytes, AcquisitionPolicy::LocalOnly)
}

pub fn snapshot_with_policy(
    path: &Path,
    max_bytes: Option<u64>,
    policy: AcquisitionPolicy,
) -> Result<Snapshot, AcquireError> {
    let read = read_verified_with_policy(path, max_bytes, policy)?;
    let hash = ContentHash(sha256_hex(&read.bytes));
    Ok(read.into_snapshot(hash))
}

/// [`snapshot`] without the hash: read `path` completely with the same
/// checks, and leave hashing to the caller.
pub fn read_verified(path: &Path, max_bytes: Option<u64>) -> Result<Unhashed, AcquireError> {
    read_verified_with_policy(path, max_bytes, AcquisitionPolicy::LocalOnly)
}

pub fn read_verified_with_policy(
    path: &Path,
    max_bytes: Option<u64>,
    policy: AcquisitionPolicy,
) -> Result<Unhashed, AcquireError> {
    let before_meta = fs::metadata(path)?;
    if !before_meta.is_file() {
        return Err(AcquireError::NotAFile);
    }
    let before = observe(&before_meta);
    if before.size == 0 {
        return Err(AcquireError::Empty);
    }
    if let Some(max) = max_bytes
        && before.size > max
    {
        return Err(AcquireError::TooLarge {
            size: before.size,
            max,
        });
    }

    // On macOS metadata does not materialize a File Provider placeholder.
    // This guard must remain before every operation that opens file contents.
    if materialization(&before_meta) == Materialization::Dataless
        && policy == AcquisitionPolicy::LocalOnly
    {
        return Err(AcquireError::Dataless);
    }

    let bytes = fs::read(path)?;

    let after_meta = fs::metadata(path)?;
    let after = observe(&after_meta);
    let read_len = bytes.len() as u64;
    if after != before || read_len != before.size {
        return Err(AcquireError::ChangedDuringRead);
    }

    let source = SourceObservation {
        path: path.to_string_lossy().into_owned(),
        inode: before.inode,
        device: before.device,
        mtime_unix: before.mtime_unix,
        size: before.size,
    };
    Ok(Unhashed { bytes, source })
}

#[cfg(test)]
mod tests {
    use tempfile::{NamedTempFile, tempdir};

    use super::*;

    #[test]
    fn snapshot_has_hash_size_and_path() {
        let file = NamedTempFile::new().unwrap();
        let content = b"%PDF-1.4 hello";
        fs::write(file.path(), content).unwrap();

        let snap = snapshot(file.path(), None).unwrap();

        assert_eq!(snap.bytes, content.to_vec());
        assert_eq!(snap.hash, ContentHash(sha256_hex(content)));
        assert_eq!(snap.source.size, content.len() as u64);
        assert_eq!(snap.source.path, file.path().to_string_lossy());
        if cfg!(unix) {
            assert!(snap.source.inode.is_some());
            assert!(snap.source.device.is_some());
            assert!(snap.source.mtime_unix.is_some());
        }
    }

    #[test]
    fn unhashed_read_matches_snapshot() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), b"%PDF-1.4 same bytes").unwrap();

        let read = read_verified(file.path(), None).unwrap();
        let snap = snapshot(file.path(), None).unwrap();
        let hash = ContentHash(sha256_hex(&read.bytes));

        assert_eq!(read.into_snapshot(hash), snap);
    }

    #[test]
    fn empty_file_is_rejected() {
        let file = NamedTempFile::new().unwrap();
        let err = snapshot(file.path(), None).unwrap_err();
        assert!(matches!(err, AcquireError::Empty), "{err:?}");
    }

    #[test]
    fn oversized_file_is_rejected() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), b"0123456789").unwrap();
        let err = snapshot(file.path(), Some(4)).unwrap_err();
        match err {
            AcquireError::TooLarge { size, max } => {
                assert_eq!(size, 10);
                assert_eq!(max, 4);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn limit_equal_to_size_is_accepted() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), b"0123456789").unwrap();
        assert!(snapshot(file.path(), Some(10)).is_ok());
    }

    #[test]
    fn directory_is_rejected() {
        let dir = tempdir().unwrap();
        let err = snapshot(dir.path(), None).unwrap_err();
        assert!(matches!(err, AcquireError::NotAFile), "{err:?}");
    }

    #[test]
    fn missing_file_is_io_error() {
        let dir = tempdir().unwrap();
        let err = snapshot(&dir.path().join("missing.pdf"), None).unwrap_err();
        assert!(matches!(err, AcquireError::Io(_)), "{err:?}");
    }

    #[test]
    fn dataless_flag_classification_is_platform_independent() {
        assert_eq!(classify_flags(0), Materialization::Local);
        assert_eq!(classify_flags(0x20), Materialization::Local);
        assert_eq!(classify_flags(UF_DATALESS), Materialization::Dataless);
        assert_eq!(
            classify_flags(UF_DATALESS | 0x20),
            Materialization::Dataless
        );
    }

    #[test]
    fn policy_defaults_to_refusing_hydration() {
        assert_eq!(AcquisitionPolicy::default(), AcquisitionPolicy::LocalOnly);
        assert_eq!(
            serde_json::to_string(&AcquisitionPolicy::AllowHydration).unwrap(),
            "\"allow_hydration\""
        );
    }

    #[test]
    fn aggregate_budget_defers_before_exhaustion() {
        let mut budget = MaterializationBudget::new(10);
        budget.reserve(6).unwrap();
        let err = budget.reserve(5).unwrap_err();
        assert!(matches!(
            err,
            AcquireError::MaterializationBudget {
                size: 5,
                remaining: 4
            }
        ));
        assert_eq!(budget.remaining(), 4);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dataless_fixture_is_rejected_before_read_when_available() {
        // CI may provide a genuine File Provider placeholder. Merely creating
        // a sparse file does not set UF_DATALESS, so absence is a valid skip.
        let Some(path) = std::env::var_os("TPE_DATALESS_FIXTURE") else {
            return;
        };
        let path = Path::new(&path);
        let meta = fs::metadata(path).unwrap();
        assert_eq!(materialization(&meta), Materialization::Dataless);
        assert!(matches!(
            read_verified(path, None),
            Err(AcquireError::Dataless)
        ));
    }
}
