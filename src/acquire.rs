//! Immutable snapshot of an input file: its complete bytes, their content
//! hash and where they were observed. The file is stat'ed before and after
//! the read so a concurrent modification is reported instead of hashed.

use std::fs;
use std::io::Read;
use std::ops::ControlFlow;
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
    /// The caller's poll answered `Break` while the file was being read.
    #[error("stopped while reading")]
    Stopped,
}

/// The file is read (and hashed) in pieces of this size, so a stop request
/// is noticed after at most one piece of a slow read.
const CHUNK: u64 = 4 << 20;

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

/// [`snapshot`] that reads and hashes in one pass and calls `poll(done,
/// total)` (bytes) after every piece; `Break` gives up with
/// [`AcquireError::Stopped`]. For a caller that must stay stoppable while a
/// slow file, such as one on a network share, is being read.
pub fn snapshot_polled(
    path: &Path,
    max_bytes: Option<u64>,
    poll: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> Result<Snapshot, AcquireError> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let read = read_impl(path, max_bytes, poll, Some(&mut hasher))?;
    let hash = ContentHash(hex::encode(hasher.finalize().as_slice()));
    Ok(read.into_snapshot(hash))
}

/// [`snapshot`] without the hash: read `path` completely with the same
/// checks, and leave hashing to the caller.
pub fn read_verified(path: &Path, max_bytes: Option<u64>) -> Result<Unhashed, AcquireError> {
    read_verified_polled(path, max_bytes, &mut |_, _| ControlFlow::Continue(()))
}

/// [`read_verified`] calling `poll(done, total)` (bytes) after every piece;
/// `Break` gives up with [`AcquireError::Stopped`].
pub fn read_verified_polled(
    path: &Path,
    max_bytes: Option<u64>,
    poll: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> Result<Unhashed, AcquireError> {
    read_impl(path, max_bytes, poll, None)
}

fn read_impl(
    path: &Path,
    max_bytes: Option<u64>,
    poll: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
    mut hasher: Option<&mut sha2::Sha256>,
) -> Result<Unhashed, AcquireError> {
    use sha2::Digest;
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

    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(before.size).unwrap_or(0));
    loop {
        let start = bytes.len();
        let got = (&mut file).take(CHUNK).read_to_end(&mut bytes)?;
        if got == 0 {
            break;
        }
        if let Some(hasher) = hasher.as_deref_mut() {
            hasher.update(&bytes[start..]);
        }
        // Longer than it was: a writer is appending, so stop reading it.
        if bytes.len() as u64 > before.size {
            return Err(AcquireError::ChangedDuringRead);
        }
        if poll(bytes.len() as u64, before.size).is_break() {
            return Err(AcquireError::Stopped);
        }
    }

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
    fn a_stop_between_pieces_gives_up_and_a_polled_hash_matches() {
        // Two full pieces and a partial one.
        let bytes: Vec<u8> = (0..(9u32 << 20)).map(|n| (n % 251) as u8).collect();
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, &bytes).unwrap();

        let mut seen = Vec::new();
        let snap = snapshot_polled(file.path(), None, &mut |done, total| {
            seen.push((done, total));
            ControlFlow::Continue(())
        })
        .unwrap();
        let total = bytes.len() as u64;
        assert_eq!(seen, [(4 << 20, total), (8 << 20, total), (total, total)]);
        assert_eq!(snap.hash, snapshot(file.path(), None).unwrap().hash);

        let mut polls = 0;
        let stopped = read_verified_polled(file.path(), None, &mut |_, _| {
            polls += 1;
            ControlFlow::Break(())
        });
        assert!(matches!(stopped, Err(AcquireError::Stopped)), "{stopped:?}");
        assert_eq!(polls, 1, "it gave up at the first piece, not at the end");
    }

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
}
