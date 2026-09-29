//! Quota-aware, crash-safe publication of generated artifacts.
//!
//! A writer is intended to be shared by every worker in a process.  Its
//! reservations serialize the decision to consume output and temporary-file
//! space, preventing a group of workers from all accepting the same snapshot
//! of the filesystem's available bytes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use thiserror::Error;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const TEMP_PREFIX: &str = ".tpe-artifact-";

/// How aggressively a successful publication is flushed to stable storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// Atomic visibility only; the operating system may flush later.
    #[default]
    Atomic,
    /// Flush file data before rename and the containing directory afterwards.
    Durable,
}

/// Limits shared by all clones of an [`ArtifactWriter`].
#[derive(Clone, Debug)]
pub struct ArtifactLimits {
    pub max_output_bytes: Option<u64>,
    pub max_temp_bytes: Option<u64>,
    pub min_free_bytes: u64,
    pub durability: Durability,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: None,
            max_temp_bytes: None,
            min_free_bytes: 0,
            durability: Durability::Atomic,
        }
    }
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error(
        "artifact output quota exceeded: requested {requested} bytes, {used} already reserved/written, limit {limit}"
    )]
    OutputQuota {
        requested: u64,
        used: u64,
        limit: u64,
    },
    #[error(
        "artifact temporary-space quota exceeded: requested {requested} bytes, {used} reserved, limit {limit}"
    )]
    TempQuota {
        requested: u64,
        used: u64,
        limit: u64,
    },
    #[error(
        "insufficient free space on {filesystem}: {available} bytes available, {required} required (including reservations and --min-free-bytes)"
    )]
    LowSpace {
        filesystem: String,
        available: u64,
        required: u64,
    },
    #[error(
        "filesystem {filesystem} does not provide the required local atomic-rename semantics for {path}"
    )]
    UnsupportedFilesystem { filesystem: String, path: PathBuf },
    #[error("artifact I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Clone, Debug)]
pub struct FilesystemInfo {
    pub name: String,
    pub available_bytes: u64,
    pub atomic_rename: bool,
}

#[derive(Default)]
struct Usage {
    output: u64,
    temp: u64,
}

#[derive(Clone)]
pub struct ArtifactWriter {
    limits: ArtifactLimits,
    usage: Arc<Mutex<Usage>>,
}

impl ArtifactWriter {
    pub fn new(limits: ArtifactLimits) -> Self {
        Self {
            limits,
            usage: Arc::new(Mutex::new(Usage::default())),
        }
    }

    /// Publish `content`, reusing an identical regular file when possible.
    pub fn write(&self, target: &Path, content: &[u8]) -> Result<(), ArtifactError> {
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        let digest: [u8; 32] = Sha256::digest(content).into();
        let info = filesystem_info(parent).map_err(|source| io_error(parent, source))?;
        validate_filesystem(target, &info)?;
        if existing_matches(target, content.len() as u64, &digest)
            .map_err(|source| io_error(target, source))?
        {
            return Ok(());
        }

        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        let reservation = self.reserve(size, &info)?;
        let temp = unique_temp(parent);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|source| io_error(&temp, source))?;
            file.write_all(content)
                .map_err(|source| io_error(&temp, source))?;
            if self.limits.durability == Durability::Durable {
                file.sync_all().map_err(|source| io_error(&temp, source))?;
            }
            drop(file);
            fs::rename(&temp, target).map_err(|source| io_error(target, source))?;
            if self.limits.durability == Durability::Durable {
                File::open(parent)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|source| io_error(parent, source))?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        reservation.finish(result.is_ok());
        result
    }

    fn reserve(&self, size: u64, fs: &FilesystemInfo) -> Result<Reservation, ArtifactError> {
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(limit) = self.limits.max_output_bytes
            && usage.output.saturating_add(size) > limit
        {
            return Err(ArtifactError::OutputQuota {
                requested: size,
                used: usage.output,
                limit,
            });
        }
        if let Some(limit) = self.limits.max_temp_bytes
            && usage.temp.saturating_add(size) > limit
        {
            return Err(ArtifactError::TempQuota {
                requested: size,
                used: usage.temp,
                limit,
            });
        }
        let required = self
            .limits
            .min_free_bytes
            .saturating_add(usage.temp)
            .saturating_add(size);
        if fs.available_bytes < required {
            return Err(ArtifactError::LowSpace {
                filesystem: fs.name.clone(),
                available: fs.available_bytes,
                required,
            });
        }
        usage.output = usage.output.saturating_add(size);
        usage.temp = usage.temp.saturating_add(size);
        Ok(Reservation {
            usage: Arc::clone(&self.usage),
            size,
            active: true,
        })
    }

    pub fn filesystem(&self, path: &Path) -> io::Result<FilesystemInfo> {
        filesystem_info(path)
    }
}

struct Reservation {
    usage: Arc<Mutex<Usage>>,
    size: u64,
    active: bool,
}
impl Reservation {
    fn finish(mut self, committed: bool) {
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        usage.temp = usage.temp.saturating_sub(self.size);
        if !committed {
            usage.output = usage.output.saturating_sub(self.size);
        }
        self.active = false;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.active {
            let mut usage = self
                .usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            usage.temp = usage.temp.saturating_sub(self.size);
            usage.output = usage.output.saturating_sub(self.size);
        }
    }
}

fn validate_filesystem(path: &Path, info: &FilesystemInfo) -> Result<(), ArtifactError> {
    if info.atomic_rename {
        return Ok(());
    }
    Err(ArtifactError::UnsupportedFilesystem {
        filesystem: info.name.clone(),
        path: path.to_path_buf(),
    })
}

fn io_error(path: &Path, source: io::Error) -> ArtifactError {
    ArtifactError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn existing_matches(path: &Path, length: u64, digest: &[u8; 32]) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !metadata.file_type().is_file() || metadata.len() != length {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().as_slice() == digest)
}

fn unique_temp(parent: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        "{TEMP_PREFIX}{}-{nonce}-{sequence}.tmp",
        std::process::id()
    ))
}

/// Remove only old, regular temporary files bearing this module's private prefix.
pub fn remove_stale_temps(root: &Path, older_than: Duration) -> io::Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(TEMP_PREFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let old = metadata
            .modified()
            .ok()
            .and_then(|time| now.duration_since(time).ok())
            .is_some_and(|age| age >= older_than);
        if old {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Recursively clean artifact temporaries without following directory symlinks.
pub fn remove_stale_temps_tree(root: &Path, older_than: Duration) -> io::Result<usize> {
    let mut removed = remove_stale_temps(root, older_than)?;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(removed),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            removed += remove_stale_temps_tree(&entry.path(), older_than)?;
        }
    }
    Ok(removed)
}

#[cfg(target_os = "linux")]
pub fn filesystem_info(path: &Path) -> io::Result<FilesystemInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &raw mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let magic = stat.f_type as u64;
    let magic_name = match magic {
        0xef53 => "ext4",
        0x5846_5342 => "xfs",
        0x9123_683e => "btrfs",
        0x0102_1994 => "tmpfs",
        0x794c_7630 => "overlay",
        0x6969 => "nfs",
        0xff53_4d42 => "cifs",
        _ => "other",
    };
    let name = linux_mount_type(path).unwrap_or_else(|| magic_name.into());
    let atomic_rename = matches!(
        name.as_str(),
        "ext4" | "xfs" | "btrfs" | "tmpfs" | "overlay"
    );
    Ok(FilesystemInfo {
        name: if name == "other" {
            format!("unknown(0x{magic:x})")
        } else {
            name
        },
        available_bytes: stat.f_bavail.saturating_mul(stat.f_bsize as u64),
        atomic_rename,
    })
}

#[cfg(target_os = "linux")]
fn linux_mount_type(path: &Path) -> Option<String> {
    let canonical = path.canonicalize().ok()?;
    let mounts = fs::read_to_string("/proc/self/mountinfo").ok()?;
    mounts
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mount = before.split_whitespace().nth(4)?.replace("\\040", " ");
            let mount = PathBuf::from(mount);
            if !canonical.starts_with(&mount) {
                return None;
            }
            let fs_type = after.split_whitespace().next()?.to_string();
            Some((mount.as_os_str().len(), fs_type))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, fs_type)| fs_type)
}

#[cfg(target_os = "macos")]
pub fn filesystem_info(path: &Path) -> io::Result<FilesystemInfo> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &raw mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let name = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(FilesystemInfo {
        atomic_rename: matches!(name.as_str(), "apfs" | "hfs"),
        name,
        available_bytes: stat.f_bavail.saturating_mul(stat.f_bsize as u64),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn filesystem_info(_path: &Path) -> io::Result<FilesystemInfo> {
    Ok(FilesystemInfo {
        name: "unsupported-host".into(),
        available_bytes: 0,
        atomic_rename: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn reuses_unchanged_and_replaces_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        let writer = ArtifactWriter::new(ArtifactLimits::default());
        writer.write(&path, b"same").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        writer.write(&path, b"same").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        writer.write(&path, b"replacement").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }

    #[test]
    fn quotas_and_low_space_are_rejected() {
        let writer = ArtifactWriter::new(ArtifactLimits {
            max_output_bytes: Some(3),
            ..ArtifactLimits::default()
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            writer.write(&dir.path().join("x"), b"four"),
            Err(ArtifactError::OutputQuota { .. })
        ));
        let writer = ArtifactWriter::new(ArtifactLimits {
            max_temp_bytes: Some(3),
            ..ArtifactLimits::default()
        });
        assert!(matches!(
            writer.write(&dir.path().join("temporary"), b"four"),
            Err(ArtifactError::TempQuota { .. })
        ));
        let writer = ArtifactWriter::new(ArtifactLimits {
            min_free_bytes: u64::MAX,
            ..ArtifactLimits::default()
        });
        assert!(matches!(
            writer.write(&dir.path().join("y"), b"x"),
            Err(ArtifactError::LowSpace { .. })
        ));
    }

    #[test]
    fn concurrent_reservations_cannot_overcommit() {
        let usage = Arc::new(Mutex::new(Usage::default()));
        let writer = ArtifactWriter {
            limits: ArtifactLimits {
                max_temp_bytes: Some(5),
                ..ArtifactLimits::default()
            },
            usage,
        };
        let info = FilesystemInfo {
            name: "ext4".into(),
            available_bytes: 100,
            atomic_rename: true,
        };
        let barrier = Arc::new(Barrier::new(3));
        let outcomes: Vec<_> = (0..2)
            .map(|_| {
                let writer = writer.clone();
                let info = info.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = writer.reserve(4, &info);
                    barrier.wait();
                    result
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        let successes = outcomes
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .count();
        assert_eq!(successes, 1);
    }

    #[test]
    fn cleanup_is_conservative_and_unsupported_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ordinary.tmp"), b"keep").unwrap();
        fs::write(dir.path().join(format!("{TEMP_PREFIX}old.tmp")), b"remove").unwrap();
        assert_eq!(remove_stale_temps(dir.path(), Duration::ZERO).unwrap(), 1);
        assert!(dir.path().join("ordinary.tmp").exists());
        let info = FilesystemInfo {
            name: "nfs".into(),
            available_bytes: 100,
            atomic_rename: false,
        };
        assert!(matches!(
            validate_filesystem(Path::new("out"), &info),
            Err(ArtifactError::UnsupportedFilesystem { .. })
        ));
    }

    #[test]
    fn failed_publication_removes_partial_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let writer = ArtifactWriter::new(ArtifactLimits::default());
        assert!(
            writer
                .write(&target, b"cannot replace a directory")
                .is_err()
        );
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(TEMP_PREFIX)
        }));
    }
}
