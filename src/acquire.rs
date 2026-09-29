//! Immutable snapshot of an input file: its complete bytes, their content
//! hash and where they were observed. The file is stat'ed before and after
//! the read so a concurrent modification is reported instead of hashed.

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use thiserror::Error;

#[cfg(test)]
use crate::schema::sha256_hex;
use crate::schema::{ContentHash, SourceObservation};
use sha2::{Digest, Sha256};

/// Hash an arbitrary reader without first collecting it into a contiguous
/// allocation.  The fixed buffer is deliberately part of this API's
/// contract: acquisition memory does not grow with the input.
pub fn hash_reader(reader: &mut impl Read) -> Result<ContentHash, std::io::Error> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(ContentHash(hex::encode(digest.finalize())))
}

/// An identity-checked file snapshot.  It can be hashed through a bounded
/// buffered reader and only materialises contiguous bytes when a backend
/// explicitly needs them.
#[derive(Debug)]
pub struct SnapshotHandle {
    file: File,
    observed: Observed,
    pub source: SourceObservation,
}

impl SnapshotHandle {
    pub fn len(&self) -> u64 {
        self.observed.size
    }

    /// Snapshot handles are never empty; acquisition rejects empty files.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn hash(&self) -> Result<ContentHash, AcquireError> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let hash = hash_reader(&mut reader)?;
        self.verify()?;
        Ok(hash)
    }

    /// Supply contiguous bytes to legacy/native backends.  Callers that can
    /// consume a reader need not pay this document-sized allocation.
    pub fn with_contiguous<T>(&self, consume: impl FnOnce(&[u8]) -> T) -> Result<T, AcquireError> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(self.observed.size as usize);
        file.read_to_end(&mut bytes)?;
        self.verify_len(bytes.len() as u64)?;
        Ok(consume(&bytes))
    }

    fn verify(&self) -> Result<(), AcquireError> {
        self.verify_len(self.observed.size)
    }

    fn verify_len(&self, read_len: u64) -> Result<(), AcquireError> {
        let after = observe(&self.file.metadata()?);
        if after != self.observed || read_len != self.observed.size {
            return Err(AcquireError::ChangedDuringRead);
        }
        Ok(())
    }
}

/// Open and identify a file without reading its contents into memory.
pub fn open_snapshot(path: &Path, max_bytes: Option<u64>) -> Result<SnapshotHandle, AcquireError> {
    let file = File::open(path)?;
    let before_meta = file.metadata()?;
    validate_metadata(&before_meta, max_bytes)?;
    let observed = observe(&before_meta);
    let source = SourceObservation {
        path: path.to_string_lossy().into_owned(),
        inode: observed.inode,
        device: observed.device,
        mtime_unix: observed.mtime_unix,
        size: observed.size,
    };
    Ok(SnapshotHandle {
        file,
        observed,
        source,
    })
}

fn validate_metadata(meta: &fs::Metadata, max_bytes: Option<u64>) -> Result<(), AcquireError> {
    if !meta.is_file() {
        return Err(AcquireError::NotAFile);
    }
    if meta.len() == 0 {
        return Err(AcquireError::Empty);
    }
    if let Some(max) = max_bytes
        && meta.len() > max
    {
        return Err(AcquireError::TooLarge {
            size: meta.len(),
            max,
        });
    }
    Ok(())
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
    let handle = open_snapshot(path, max_bytes)?;
    let hash = handle.hash()?;
    let source = handle.source.clone();
    handle.with_contiguous(|bytes| Snapshot {
        bytes: bytes.to_vec(),
        hash,
        source,
    })
}

/// [`snapshot`] without the hash: read `path` completely with the same
/// checks, and leave hashing to the caller.
pub fn read_verified(path: &Path, max_bytes: Option<u64>) -> Result<Unhashed, AcquireError> {
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
    fn handle_hashes_without_materialising_and_can_supply_contiguous_bytes() {
        let file = NamedTempFile::new().unwrap();
        let content = vec![0x5a; 3 * 64 * 1024 + 17];
        fs::write(file.path(), &content).unwrap();

        let handle = open_snapshot(file.path(), None).unwrap();
        assert_eq!(handle.len(), content.len() as u64);
        assert_eq!(handle.hash().unwrap(), ContentHash(sha256_hex(&content)));
        let observed = handle
            .with_contiguous(|bytes| (bytes.len(), sha256_hex(bytes)))
            .unwrap();
        assert_eq!(observed, (content.len(), sha256_hex(&content)));
    }

    #[test]
    fn reader_hash_uses_streaming_reads() {
        struct SmallReads<'a> {
            remaining: &'a [u8],
            largest: usize,
        }
        impl Read for SmallReads<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(997).min(self.remaining.len());
                self.largest = self.largest.max(count);
                output[..count].copy_from_slice(&self.remaining[..count]);
                self.remaining = &self.remaining[count..];
                Ok(count)
            }
        }
        let input = vec![7; 2 * 1024 * 1024];
        let mut reader = SmallReads {
            remaining: &input,
            largest: 0,
        };
        assert_eq!(hash_reader(&mut reader).unwrap().0, sha256_hex(&input));
        assert!(reader.largest <= 997);
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
