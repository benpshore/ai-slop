//! Durable, content-addressed intake for unattended PDF extraction.
//!
//! Paths and file IDs are deliberately observations, never document keys.

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::match_bool,
    clippy::wildcard_imports,
    clippy::unnecessary_wraps
)]

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tpe::acquire;
use tpe::ledger::Ledger;
use tpe::schema::{ContentHash, Job, SourceObservation};

fn log(level: &str, message: &str, path: Option<&Path>) {
    eprintln!(
        "{}",
        serde_json::json!({"level": level, "message": message, "path": path.map(|p| p.to_string_lossy())})
    );
}

/// Persisted lifecycle of content in the intake queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Discovered,
    Stabilizing,
    Queued,
    Processing,
    Complete,
    Partial,
    Failed,
    Cancelled,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Stabilizing => "stabilizing",
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Runtime configuration. Durations are milliseconds and sizes are bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub watched_folders: Vec<PathBuf>,
    pub output_location: PathBuf,
    pub ledger_location: PathBuf,
    pub backend: String,
    pub concurrency: usize,
    pub input_size_limit: u64,
    pub stable_checks: u32,
    pub stable_interval_ms: u64,
    pub retry_attempts: u32,
    pub retry_delay_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watched_folders: vec![],
            output_location: "output".into(),
            ledger_location: "watchdog.sqlite".into(),
            backend: "lopdf".into(),
            concurrency: 2,
            input_size_limit: 512 * 1024 * 1024,
            stable_checks: 3,
            stable_interval_ms: 750,
            retry_attempts: 3,
            retry_delay_ms: 2_000,
        }
    }
}

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS intake (hash TEXT PRIMARY KEY, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, error TEXT, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS observations (id INTEGER PRIMARY KEY, hash TEXT NOT NULL REFERENCES intake(hash), path TEXT NOT NULL, inode INTEGER, device INTEGER, mtime_unix INTEGER, size INTEGER NOT NULL, seen_at INTEGER NOT NULL, UNIQUE(hash,path,inode,device,mtime_unix,size));
CREATE INDEX IF NOT EXISTS observations_hash ON observations(hash);
";

/// SQLite-backed intake journal, separate from the extraction ledger.
pub struct IntakeLedger {
    conn: Connection,
}

impl IntakeLedger {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        // A crash cannot leave work permanently processing.
        conn.execute(
            "UPDATE intake SET state='queued', updated_at=?1 WHERE state='processing'",
            [now()],
        )?;
        Ok(Self { conn })
    }

    pub fn observe(&mut self, hash: &ContentHash, source: &SourceObservation) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO intake(hash,state,updated_at) VALUES(?1,'queued',?2)",
            params![hash.0, now()],
        )? != 0;
        let inode = source.inode.and_then(|v| i64::try_from(v).ok());
        let device = source.device.and_then(|v| i64::try_from(v).ok());
        tx.execute("INSERT OR IGNORE INTO observations(hash,path,inode,device,mtime_unix,size,seen_at) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![hash.0, source.path, inode, device, source.mtime_unix, i64::try_from(source.size).unwrap_or(i64::MAX), now()])?;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn transition(
        &mut self,
        hash: &ContentHash,
        state: State,
        error: Option<&str>,
    ) -> Result<()> {
        let changed = self.conn.execute("UPDATE intake SET state=?2,error=?3,updated_at=?4,attempts=attempts+CASE WHEN ?2='processing' THEN 1 ELSE 0 END WHERE hash=?1", params![hash.0, state.as_str(), error, now()])?;
        if changed == 0 {
            return Err(anyhow!("unknown intake hash {}", hash.0));
        }
        Ok(())
    }

    pub fn state(&self, hash: &ContentHash) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT state FROM intake WHERE hash=?1", [&hash.0], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn recover(&self) -> Result<Vec<(ContentHash, PathBuf)>> {
        let mut stmt = self.conn.prepare("SELECT i.hash,o.path FROM intake i JOIN observations o ON o.hash=i.hash WHERE i.state='queued' AND o.id=(SELECT MAX(id) FROM observations WHERE hash=i.hash)")?;
        Ok(stmt
            .query_map([], |r| {
                Ok((
                    ContentHash(r.get(0)?),
                    PathBuf::from(r.get::<_, String>(1)?),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn hidden_or_package(path: &Path) -> bool {
    let Some(Component::Normal(name)) = path.components().next_back() else {
        return false;
    };
    let name = name.to_string_lossy();
    name.starts_with('.')
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "__macosx" | "node_modules"
        )
        || name.to_ascii_lowercase().ends_with(".app")
}

/// Reject artifacts that should never enter document identity.
pub fn eligible(path: &Path, config: &Config) -> bool {
    if hidden_or_package(path)
        || path.starts_with(&config.output_location)
        || path == config.ledger_location
    {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        && !name.ends_with(".part.pdf")
        && !name.ends_with(".partial.pdf")
        && !name.starts_with("~$")
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// Wait until repeated metadata observations and complete open/read checks agree.
pub fn wait_stable(path: &Path, config: &Config) -> Result<tpe::acquire::Snapshot> {
    let mut previous = None;
    let mut matches = 0;
    loop {
        let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let stamp = Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        if stamp.len > config.input_size_limit {
            return Err(anyhow!("input exceeds {} bytes", config.input_size_limit));
        }
        let mut file = fs::File::open(path)?;
        let bytes_read = std::io::copy(
            &mut file.by_ref().take(config.input_size_limit + 1),
            &mut std::io::sink(),
        )?;
        let after = fs::metadata(path)?;
        let unchanged = after.len() == stamp.len
            && after.modified().ok() == stamp.modified
            && bytes_read == stamp.len;
        match unchanged {
            true if previous == Some(stamp) => {
                matches += 1;
                if matches >= config.stable_checks.saturating_sub(1) {
                    // Only call acquire after the repeated stat/open/read gate.
                    return Ok(acquire::snapshot(path, Some(config.input_size_limit))?);
                }
            }
            true => {
                matches = 0;
                previous = Some(stamp);
            }
            false => {
                matches = 0;
                previous = None;
            }
        }
        thread::sleep(Duration::from_millis(config.stable_interval_ms));
    }
}

/// Recursively enumerate all eligible files. Used at startup and whenever an
/// event is dropped/coalesced; periodic use also protects against backend gaps.
pub fn reconcile(config: &Config) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, config: &Config, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                if !hidden_or_package(&path) {
                    walk(&path, config, out)?;
                }
            } else if eligible(&path, config) {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut found = vec![];
    for root in &config.watched_folders {
        walk(root, config, &mut found)?;
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// Run one durable bounded-queue service. macOS uses directory-level `FSEvents`.
pub fn run(config: Config, stop: Receiver<()>) -> Result<()> {
    fs::create_dir_all(&config.output_location)?;
    let intake_path = config.ledger_location.with_extension("intake.sqlite");
    let mut intake = IntakeLedger::open(&intake_path)?;
    let event_rx = platform_watch::watch(&config.watched_folders)?;
    let (work_tx, work_rx) =
        mpsc::sync_channel::<(ContentHash, PathBuf)>(config.concurrency.max(1) * 2);
    let (done_tx, done_rx) = mpsc::channel();
    let shared_rx = std::sync::Arc::new(std::sync::Mutex::new(work_rx));
    for _ in 0..config.concurrency.max(1) {
        let rx = shared_rx.clone();
        let done = done_tx.clone();
        let cfg = config.clone();
        thread::spawn(move || worker(rx, done, cfg));
    }
    let mut submitted = HashSet::new();
    let mut candidates = reconcile(&config)?;
    for (hash, path) in intake.recover()? {
        submitted.insert(hash.0.clone());
        intake.transition(&hash, State::Processing, None)?;
        work_tx.send((hash, path))?;
    }
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        while let Ok((hash, state, error)) = done_rx.try_recv() {
            intake.transition(&hash, state, error.as_deref())?;
            submitted.remove(&hash.0);
        }
        while let Some(path) = candidates.pop() {
            ingest(&config, &mut intake, &work_tx, &mut submitted, path)?;
        }
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            // FSEvents is intentionally only a hint: every batch is followed
            // by a full reconciliation, which handles MustScanSubDirs,
            // KernelDropped/UserDropped, coalescing, and directory renames.
            Ok(()) => candidates.extend(reconcile(&config)?),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("watcher disconnected"));
            }
        }
    }
    drop(work_tx);
    Ok(())
}

fn ingest(
    config: &Config,
    intake: &mut IntakeLedger,
    tx: &SyncSender<(ContentHash, PathBuf)>,
    submitted: &mut HashSet<String>,
    path: PathBuf,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let snapshot = match wait_stable(&path, config) {
        Ok(value) => value,
        Err(error) => {
            log(
                "warn",
                &format!("input did not stabilize: {error}"),
                Some(&path),
            );
            return Ok(());
        }
    };
    let hash = snapshot.hash.clone();
    let is_new = intake.observe(&hash, &snapshot.source)?;
    if is_new && submitted.insert(hash.0.clone()) {
        // Commit processing before exposing work to a worker. A crash after
        // this point is recovered to queued by IntakeLedger::open.
        intake.transition(&hash, State::Processing, None)?;
        tx.send((hash, path))?;
    }
    Ok(())
}

fn worker(
    rx: std::sync::Arc<std::sync::Mutex<Receiver<(ContentHash, PathBuf)>>>,
    done: mpsc::Sender<(ContentHash, State, Option<String>)>,
    config: Config,
) {
    loop {
        let Ok((hash, path)) = rx.lock().expect("worker queue poisoned").recv() else {
            break;
        };
        let mut outcome = Err(anyhow!("not attempted"));
        for attempt in 0..config.retry_attempts.max(1) {
            let job = Job {
                path: path.to_string_lossy().into_owned(),
                backend: config.backend.clone(),
                pages: None,
                password: None,
                max_bytes: Some(config.input_size_limit),
                figures_dir: Some(config.output_location.to_string_lossy().into_owned()),
            };
            outcome = tpe::pipeline::run_job(&job)
                .map_err(anyhow::Error::from)
                .and_then(|result| {
                    let state = if result.status == tpe::schema::Status::Partial {
                        State::Partial
                    } else {
                        State::Complete
                    };
                    let mut ledger =
                        Ledger::open(&config.ledger_location).map_err(|e| anyhow!(e))?;
                    ledger.write_result(&result).map_err(|e| anyhow!(e))?;
                    Ok(state)
                });
            if outcome.is_ok() {
                break;
            }
            if attempt + 1 < config.retry_attempts.max(1) {
                thread::sleep(Duration::from_millis(config.retry_delay_ms));
            }
        }
        match outcome {
            Ok(state) => {
                let _ = done.send((hash, state, None));
            }
            Err(error) => {
                let _ = done.send((hash, State::Failed, Some(error.to_string())));
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform_watch {
    use super::*;

    /// Other platforms use reconciliation ticks. macOS has the native
    /// `FSEvents` implementation below rather than per-file notifications.
    pub fn watch(_roots: &[PathBuf]) -> Result<Receiver<()>> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(5));
                if tx.send(()).is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }
}

#[cfg(target_os = "macos")]
mod platform_watch {
    use super::*;
    use std::ffi::{CString, c_char, c_double, c_void};
    use std::ptr;

    type CFRef = *const c_void;
    type Stream = *mut c_void;
    type EventId = u64;
    type Flags = u32;
    const UTF8: u32 = 0x0800_0100;
    const SINCE_NOW: EventId = 0xffff_ffff_ffff_ffff;
    const FILE_EVENTS: Flags = 0x10;

    #[repr(C)]
    struct Context {
        version: isize,
        info: *mut c_void,
        retain: CFRef,
        release: CFRef,
        description: CFRef,
    }

    type Callback =
        unsafe extern "C" fn(Stream, *mut c_void, usize, *mut c_void, *const Flags, *const EventId);

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSEventStreamCreate(
            allocator: CFRef,
            callback: Callback,
            context: *mut Context,
            paths: CFRef,
            since: EventId,
            latency: c_double,
            flags: Flags,
        ) -> Stream;
        fn FSEventStreamScheduleWithRunLoop(stream: Stream, run_loop: CFRef, mode: CFRef);
        fn FSEventStreamStart(stream: Stream) -> bool;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: CFRef;
        fn CFStringCreateWithCString(allocator: CFRef, text: *const c_char, encoding: u32)
        -> CFRef;
        fn CFArrayCreate(
            allocator: CFRef,
            values: *const CFRef,
            count: isize,
            callbacks: CFRef,
        ) -> CFRef;
        fn CFRunLoopGetCurrent() -> CFRef;
        fn CFRunLoopRun();
    }

    unsafe extern "C" fn callback(
        _stream: Stream,
        info: *mut c_void,
        _count: usize,
        _paths: *mut c_void,
        _flags: *const Flags,
        _ids: *const EventId,
    ) {
        // SAFETY: info owns a Sender for the process lifetime of this watcher.
        let sender = unsafe { &*(info.cast::<mpsc::Sender<()>>()) };
        let _ = sender.send(());
    }

    /// Create a directory-level FSEventStream. The callback deliberately
    /// discards individual paths and asks the owner to reconcile the tree.
    pub fn watch(roots: &[PathBuf]) -> Result<Receiver<()>> {
        let (tx, rx) = mpsc::channel();
        let roots = roots.to_vec();
        thread::spawn(move || unsafe {
            let strings: Vec<CFRef> = roots
                .iter()
                .filter_map(|path| CString::new(path.to_string_lossy().as_bytes()).ok())
                .map(|path| CFStringCreateWithCString(ptr::null(), path.as_ptr(), UTF8))
                .collect();
            let paths = CFArrayCreate(
                ptr::null(),
                strings.as_ptr(),
                strings.len() as isize,
                ptr::null(),
            );
            let info = Box::into_raw(Box::new(tx)).cast();
            let mut context = Context {
                version: 0,
                info,
                retain: ptr::null(),
                release: ptr::null(),
                description: ptr::null(),
            };
            let stream = FSEventStreamCreate(
                ptr::null(),
                callback,
                &mut context,
                paths,
                SINCE_NOW,
                0.25,
                FILE_EVENTS,
            );
            if stream.is_null() {
                return;
            }
            FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            if FSEventStreamStart(stream) {
                CFRunLoopRun();
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests;
