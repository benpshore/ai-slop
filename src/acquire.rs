//! Immutable snapshot of an input file: its complete bytes, their content
//! hash and where they were observed. The file is stat'ed before and after
//! the read so a concurrent modification is reported instead of hashed.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use thiserror::Error;

use crate::schema::{ContentHash, SourceObservation, sha256_hex};

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
    let read = read_verified(path, max_bytes)?;
    let hash = ContentHash(sha256_hex(&read.bytes));
    Ok(read.into_snapshot(hash))
}

/// [`snapshot`] without the hash: read `path` completely with the same
/// checks, and leave hashing to the caller.
pub fn read_verified(path: &Path, max_bytes: Option<u64>) -> Result<Unhashed, AcquireError> {
    // Avoid opening known devices/FIFOs. The descriptor is checked again after
    // opening, so replacing a regular path cannot turn this into a device read.
    let before_meta = fs::metadata(path)?;
    if !before_meta.is_file() {
        return Err(AcquireError::NotAFile);
    }
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(AcquireError::NotAFile);
    }
    let before = observe(&opened);
    if before != observe(&before_meta) {
        return Err(AcquireError::ChangedDuringRead);
    }
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

    read_contents(&mut file, path, before, max_bytes)
}

fn read_contents(
    file: &mut File,
    path: &Path,
    before: Observed,
    max_bytes: Option<u64>,
) -> Result<Unhashed, AcquireError> {
    // Bound the read itself, not just the metadata observed before it. Even
    // without a caller cap, a concurrently growing file may not cause an
    // allocation beyond the original size plus the sentinel byte.
    let cap = max_bytes.map_or(before.size, |limit| limit.min(before.size));
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if let Some(max) = max_bytes
        && bytes.len() as u64 > max
    {
        return Err(AcquireError::TooLarge {
            size: bytes.len() as u64,
            max,
        });
    }

    let after = observe(&file.metadata()?);
    let path_after = observe(&fs::metadata(path)?);
    let read_len = bytes.len() as u64;
    if after != before || path_after != before || read_len != before.size {
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
    fn growth_after_descriptor_observation_is_bounded_and_rejected() {
        use std::io::{Seek, Write};
        let mut input = NamedTempFile::new().unwrap();
        input.write_all(b"12345678").unwrap();
        let mut opened = File::open(input.path()).unwrap();
        let before = observe(&opened.metadata().unwrap());
        input.write_all(&vec![b'x'; 1024 * 1024]).unwrap();
        let error = read_contents(&mut opened, input.path(), before, Some(8)).unwrap_err();
        assert!(matches!(error, AcquireError::TooLarge { size: 9, max: 8 }));
        assert_eq!(opened.stream_position().unwrap(), 9);
    }

    #[test]
    #[cfg(unix)]
    fn replacing_the_path_does_not_change_the_open_snapshot_identity() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("source.pdf");
        fs::write(&path, b"original").unwrap();
        let mut opened = File::open(&path).unwrap();
        let before = observe(&opened.metadata().unwrap());
        fs::rename(&path, dir.path().join("original.pdf")).unwrap();
        fs::write(&path, b"replaced").unwrap();
        let error = read_contents(&mut opened, &path, before, Some(64)).unwrap_err();
        assert!(matches!(error, AcquireError::ChangedDuringRead));
        assert_eq!(
            fs::read(dir.path().join("original.pdf")).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(&path).unwrap(), b"replaced");
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
}
