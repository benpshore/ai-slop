//! The app's model and its engine calls. No GPUI here, so this compiles and
//! is tested on every platform; the GUI (`gui.rs` in the binary) only wires
//! it to buttons, drops and a job list.
//!
//! One job is one PDF and one [`Action`]. [`run`] executes it in this
//! process (no subprocess, no serialisation) through the engine's observed
//! entry points, reporting a [`Progress`] event per page, and moves the
//! outputs next to the source only when the job is complete. A panic inside
//! the engine fails that job only.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use futures::channel::mpsc::UnboundedSender;

use tpe::acquire;
use tpe::backend::{self, BackendError};
use tpe::bibliography::{self, Record};
use tpe::ledger::Ledger;
use tpe::pipeline::{self, PipelineError, Progress};
use tpe::schema::{Job as EngineJob, Status};

/// The extraction backend every job uses.
const BACKEND: &str = "lopdf";

/// One of the two things the app does to a PDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `tpe extract`: the ordered page text, written as `<name>.txt`.
    Text,
    /// `tpe bibliography`: the final reference list, written as
    /// `<name>.references.json` and `<name>.references.txt`.
    Bibliography,
}

impl Action {
    /// Button and menu title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Text => "Get text",
            Self::Bibliography => "Get bibliography",
        }
    }

    /// Short label for a job row.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Bibliography => "bibliography",
        }
    }
}

/// What a finished job produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Files written next to the source PDF.
    pub outputs: Vec<PathBuf>,
    /// One line for the job row.
    pub summary: String,
    /// Engine warnings worth showing (never empty for a partial extraction).
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Queued,
    Running,
    Finished(Outcome),
    Failed(String),
    /// Stopped by the person; nothing was written.
    Cancelled,
}

/// Why a job produced nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The person stopped it (see [`run`]).
    Cancelled,
    /// The engine or the file system said no.
    Failed(String),
}

impl From<String> for RunError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<PipelineError> for RunError {
    fn from(error: PipelineError) -> Self {
        match error {
            PipelineError::Cancelled => Self::Cancelled,
            other => Self::Failed(other.to_string()),
        }
    }
}

impl From<BackendError> for RunError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::Cancelled => Self::Cancelled,
            other => Self::Failed(other.to_string()),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// One row of the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRow {
    pub id: usize,
    pub source: PathBuf,
    pub action: Action,
    pub phase: Phase,
    /// Pages processed so far and how many the run covers (`None` until opened).
    pub done: u32,
    pub total: Option<u32>,
    /// A stop was requested and the engine has not reached it yet.
    pub cancelling: bool,
}

impl JobRow {
    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Queued | Phase::Running)
    }

    /// The file's name for the row title.
    pub fn name(&self) -> String {
        self.source.file_name().map_or_else(
            || self.source.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// `0.0..=1.0` while the page count is known.
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.filter(|t| *t > 0)?;
        Some((self.done as f32 / total as f32).min(1.0))
    }

    /// One line under the file name: `<noun> · <state>`.
    pub fn status_line(&self) -> String {
        let state = match &self.phase {
            Phase::Queued => "Waiting".to_string(),
            Phase::Running if self.cancelling => "Cancelling".to_string(),
            Phase::Running => match self.total {
                Some(total) => format!("Page {} of {total}", self.done),
                None => "Opening".to_string(),
            },
            Phase::Finished(outcome) => match outcome.warnings.first() {
                Some(warning) => format!("{} · {warning}", outcome.summary),
                None => outcome.summary.clone(),
            },
            Phase::Failed(message) => format!("Failed: {message}"),
            Phase::Cancelled => "Cancelled".to_string(),
        };
        format!("{} · {state}", self.action.noun())
    }

    /// Files written next to the source, if the job finished.
    pub fn outputs(&self) -> &[PathBuf] {
        match &self.phase {
            Phase::Finished(outcome) => &outcome.outputs,
            _ => &[],
        }
    }

    /// The output whose text a Copy button puts on the clipboard.
    pub fn copyable(&self) -> Option<&Path> {
        self.outputs()
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "txt"))
            .map(PathBuf::as_path)
    }
}

/// A move of the selection through the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
    First,
    Last,
}

/// The rows of the window, in arrival order, and the queue discipline: one
/// job runs at a time (the ledger has one writer).
#[derive(Debug, Default)]
pub struct JobList {
    rows: Vec<JobRow>,
    next_id: usize,
}

impl JobList {
    pub fn rows(&self) -> &[JobRow] {
        &self.rows
    }

    pub fn row(&self, id: usize) -> Option<&JobRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    fn row_mut(&mut self, id: usize) -> Option<&mut JobRow> {
        self.rows.iter_mut().find(|row| row.id == id)
    }

    /// Add one row per PDF file (anything else is ignored) and return how
    /// many were added.
    pub fn enqueue(&mut self, paths: impl IntoIterator<Item = PathBuf>, action: Action) -> usize {
        let mut added = 0;
        for path in paths {
            let is_pdf = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
            if !is_pdf || !path.is_file() {
                continue;
            }
            self.next_id += 1;
            self.rows.push(JobRow {
                id: self.next_id,
                source: path,
                action,
                phase: Phase::Queued,
                done: 0,
                total: None,
                cancelling: false,
            });
            added += 1;
        }
        added
    }

    pub fn is_running(&self) -> bool {
        self.rows.iter().any(|row| row.phase == Phase::Running)
    }

    /// The oldest queued row, if no row is running.
    pub fn next_queued(&self) -> Option<usize> {
        if self.is_running() {
            return None;
        }
        self.rows
            .iter()
            .find(|row| row.phase == Phase::Queued)
            .map(|row| row.id)
    }

    pub fn start(&mut self, id: usize) {
        if let Some(row) = self.row_mut(id) {
            row.phase = Phase::Running;
        }
    }

    /// Apply a progress event; ignored once the row is no longer running.
    pub fn progress(&mut self, id: usize, event: Progress) {
        let Some(row) = self.row_mut(id) else { return };
        if row.phase != Phase::Running {
            return;
        }
        match event {
            Progress::Opened { total, .. } => row.total = Some(total),
            Progress::Page { done, total, .. } => {
                row.done = done;
                row.total = Some(total);
            }
            Progress::Reading { .. } => {}
        }
    }

    pub fn finish(&mut self, id: usize, result: Result<Outcome, RunError>) {
        if let Some(row) = self.row_mut(id) {
            row.cancelling = false;
            row.phase = match result {
                Ok(outcome) => Phase::Finished(outcome),
                Err(RunError::Cancelled) => Phase::Cancelled,
                Err(RunError::Failed(message)) => Phase::Failed(message),
            };
        }
    }

    /// Note that a stop was requested for running row `id`; `false` when it
    /// is not running (a queued row is removed instead, a finished one has
    /// nothing to stop).
    pub fn mark_cancelling(&mut self, id: usize) -> bool {
        match self.row_mut(id) {
            Some(row) if row.phase == Phase::Running => {
                row.cancelling = true;
                true
            }
            _ => false,
        }
    }

    /// Drop a queued row. A running job cannot be stopped; `false` then.
    pub fn remove(&mut self, id: usize) -> bool {
        let before = self.rows.len();
        self.rows
            .retain(|row| !(row.id == id && row.phase == Phase::Queued));
        self.rows.len() != before
    }

    pub fn has_done(&self) -> bool {
        self.rows.iter().any(|row| !row.is_active())
    }

    /// Position of row `id` in the list.
    pub fn index_of(&self, id: usize) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    /// The row a selection lands on after `step` from `current` (`None`:
    /// nothing selected). Up and Down stop at the ends rather than wrap; with
    /// nothing selected Down lands on the first row and Up on the last.
    /// `None` only when the list is empty.
    pub fn stepped(&self, current: Option<usize>, step: Step) -> Option<usize> {
        let last = self.rows.len().checked_sub(1)?;
        let at = current.and_then(|id| self.index_of(id));
        let target = match (step, at) {
            (Step::First, _) | (Step::Down, None) => 0,
            (Step::Last, _) | (Step::Up, None) => last,
            (Step::Up, Some(i)) => i.saturating_sub(1),
            (Step::Down, Some(i)) => (i + 1).min(last),
        };
        self.rows.get(target).map(|row| row.id)
    }

    /// The row to select when the one at `index` is gone: the row now at that
    /// position, else the last, else none.
    pub fn nearest_to(&self, index: usize) -> Option<usize> {
        self.rows
            .get(index)
            .or_else(|| self.rows.last())
            .map(|row| row.id)
    }

    /// Whether any row is queued or running.
    pub fn has_active(&self) -> bool {
        self.rows.iter().any(JobRow::is_active)
    }

    /// Drop finished and failed rows.
    pub fn clear_done(&mut self) {
        self.rows.retain(JobRow::is_active);
    }
}

/// The names one generation of outputs takes next to `source`: the source's
/// base name plus each suffix, `paper.txt` for generation 1 and
/// `paper 2.txt`, `paper 3.txt`, … after.
fn generation_paths(source: &Path, suffixes: &[&str], generation: u64) -> Vec<PathBuf> {
    let directory = source.parent().unwrap_or_else(|| Path::new(""));
    let base = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    suffixes
        .iter()
        .map(|suffix| {
            if generation == 1 {
                directory.join(format!("{base}{suffix}"))
            } else {
                directory.join(format!("{base} {generation}{suffix}"))
            }
        })
        .collect()
}

/// Temporary files removed when dropped, so a failure or a clash leaves none.
struct Staged(Vec<PathBuf>);

impl Staged {
    /// Remove the files now; nothing is left to remove on drop.
    fn discard(&mut self) {
        for path in self.0.drain(..) {
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.discard();
    }
}

/// Distinguishes the temporary names of concurrent publishers in one process.
static STAGING: AtomicU64 = AtomicU64::new(0);

/// Write `files` (suffix and bytes) next to `source` and return their paths.
///
/// Never overwrites: the files of one call share a generation number, and
/// any existing name in the set moves the whole set on (`paper 2.*`), so a
/// pair never mixes two runs. Each file is written completely (once) to a
/// hidden temporary name in the same directory and then given its final name
/// with a hard link, which fails instead of replacing an existing file, so a
/// reader or a force-quit sees either no file or a whole one, and two
/// concurrent publishers cannot take the same name. A clash costs another
/// link, not another write. Where the file system has no hard links (the
/// link fails as unsupported or not permitted), the final name is created
/// exclusively and written in place instead (a force-quit mid-write can then
/// leave a short file). Any other error, a full disk included, is returned
/// as it is, not retried.
///
/// # Errors
/// The file system's error, with nothing left behind that this call made.
pub fn publish(source: &Path, files: &[(&str, Vec<u8>)]) -> std::io::Result<Vec<PathBuf>> {
    let suffixes: Vec<&str> = files.iter().map(|(suffix, _)| *suffix).collect();
    let mut staged = stage(source, files)?;
    for generation in 1u64.. {
        let finals = generation_paths(source, &suffixes, generation);
        if !staged.0.is_empty() {
            match link_all(&staged.0, &finals) {
                Ok(()) => return Ok(finals),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) if links_unsupported(&error) => staged.discard(),
                Err(error) => return Err(error),
            }
        }
        match place_exclusively(&finals, files) {
            Ok(()) => return Ok(finals),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!("the generation counter does not run out")
}

/// Write each file completely to its own hidden temporary name.
fn stage(source: &Path, files: &[(&str, Vec<u8>)]) -> std::io::Result<Staged> {
    let directory = source.parent().unwrap_or_else(|| Path::new(""));
    let mut staged = Staged(Vec::new());
    for (suffix, bytes) in files {
        // A name a force-quit left behind (a later process can get the same
        // PID and restart the counter) is skipped, not an error.
        let (mut file, temporary) = loop {
            let unique = STAGING.fetch_add(1, Ordering::Relaxed);
            let temporary = directory.join(format!(
                ".pdftextract-{}-{unique}{suffix}.partial",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => break (file, temporary),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };
        staged.0.push(temporary);
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    Ok(staged)
}

/// Link each staged file to its final name; on any failure remove the links
/// this call made.
fn link_all(staged: &[PathBuf], finals: &[PathBuf]) -> std::io::Result<()> {
    let mut linked: Vec<&PathBuf> = Vec::new();
    for (temporary, target) in staged.iter().zip(finals) {
        if let Err(error) = fs::hard_link(temporary, target) {
            for done in linked {
                let _ = fs::remove_file(done);
            }
            return Err(error);
        }
        linked.push(target);
    }
    Ok(())
}

/// Whether a failed `hard_link` says this file system has no hard links,
/// as opposed to a clash or a real I/O error: FAT, exFAT and some network
/// shares answer "not supported" or "not permitted" (`EPERM`; on macOS
/// `ENOTSUP`). The directory is known to be writable by then, since the
/// staged files were just created in it.
fn links_unsupported(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{PermissionDenied, Unsupported};
    matches!(error.kind(), Unsupported | PermissionDenied)
        || (cfg!(target_os = "macos") && error.raw_os_error() == Some(45))
}

/// Create each final name exclusively and write it; on any failure remove
/// what this attempt created.
fn place_exclusively(finals: &[PathBuf], files: &[(&str, Vec<u8>)]) -> std::io::Result<()> {
    let mut created = Staged(Vec::new());
    for (target, (_, bytes)) in finals.iter().zip(files) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        created.0.push(target.clone());
        file.write_all(bytes)?;
    }
    created.0.clear(); // success: keep them
    Ok(())
}

/// Paths handed to the app from outside its window (Finder Services, files
/// opened with the app, the command line), possibly before the window
/// exists: macOS delivers launch-time opens before the application has
/// finished launching. Items sent before [`Mailbox::install`] are kept and
/// delivered, in order, the moment a sender arrives.
pub struct Mailbox<T> {
    sender: OnceLock<UnboundedSender<T>>,
    pending: Mutex<Vec<T>>,
}

impl<T> Mailbox<T> {
    pub const fn new() -> Self {
        Self {
            sender: OnceLock::new(),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Deliver `item` now, or keep it until a sender is installed.
    pub fn send(&self, item: T) {
        // The lock covers the sender check so an install cannot slip in
        // between the check and the push.
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        match self.sender.get() {
            Some(sender) => {
                let _ = sender.unbounded_send(item);
            }
            None => pending.push(item),
        }
    }

    /// Install the sender (once; later calls are ignored) and deliver
    /// everything kept so far.
    pub fn install(&self, sender: UnboundedSender<T>) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if self.sender.set(sender).is_err() {
            return;
        }
        if let Some(sender) = self.sender.get() {
            for item in pending.drain(..) {
                let _ = sender.unbounded_send(item);
            }
        }
    }
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// The ledger `tpe extract` requires: under Application Support on macOS,
/// under `$XDG_DATA_HOME` (or `~/.local/share`) elsewhere.
pub fn default_ledger_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".local/share")))
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("PDFTextract")
        .join("ledger.sqlite")
}

/// The path of a `file://` URL (as macOS hands them to `on_open_urls`), with
/// percent-escapes decoded. `None` for any other scheme or a bad escape.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // `file:///Users/..` has an empty host; `file://localhost/Users/..` names it.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(slash) if &rest[..slash] == "localhost" => &rest[slash..],
        _ => return None,
    };
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            decoded.push(value);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(decoded).ok()?))
}

/// The message for a caught panic.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// A job's stop switch. The person asks with [`CancelToken::request`]; the
/// job stops at its next page, or at the last moment before it writes
/// anything. Once the job has started writing its results (the ledger, then
/// the files) a stop is refused, so "Cancelling" is only ever shown for a job
/// that will really write nothing, and a job that was told to stop never
/// half-writes.
#[derive(Debug, Default)]
pub struct CancelToken(AtomicU8);

impl CancelToken {
    const RUNNING: u8 = 0;
    const STOP: u8 = 1;
    const COMMITTED: u8 = 2;

    pub const fn new() -> Self {
        Self(AtomicU8::new(Self::RUNNING))
    }

    /// Ask the job to stop. `true`: it will stop (or was already asked to).
    /// `false`: it is already writing its results and will finish.
    pub fn request(&self) -> bool {
        match self.0.compare_exchange(
            Self::RUNNING,
            Self::STOP,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(now) => now == Self::STOP,
        }
    }

    /// Whether a stop has been asked for.
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::STOP
    }

    /// The job's point of no return, called before its first write: `true`
    /// to go on (later stop requests are refused), `false` when a stop was
    /// asked for first and nothing may be written.
    fn commit(&self) -> bool {
        match self.0.compare_exchange(
            Self::RUNNING,
            Self::COMMITTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(now) => now == Self::COMMITTED,
        }
    }
}

/// Run one job in this process. `observe` is called on this thread for the
/// open and for each page. Setting `cancel` stops the job at the next page
/// (the post-processing of a finished parse cannot be interrupted, so a
/// stop requested during it is honoured just before anything is written).
/// Outputs are written next to `source` only on success; a failed or
/// cancelled job (including an engine panic) writes nothing, not even to the
/// ledger.
///
/// # Errors
/// [`RunError::Cancelled`] when stopped, [`RunError::Failed`] otherwise.
pub fn run(
    action: Action,
    source: &Path,
    ledger: &Path,
    observe: &mut dyn FnMut(Progress),
    cancel: &CancelToken,
) -> Result<Outcome, RunError> {
    // A stop asked for before the task got its turn: do not read the file.
    if cancel.is_requested() {
        return Err(RunError::Cancelled);
    }
    let mut watch = |event: Progress| {
        observe(event);
        if cancel.is_requested() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let result = match action {
        Action::Text => run_text(source, ledger, &mut watch, cancel),
        Action::Bibliography => run_bibliography(source, &mut watch, cancel),
    };
    // Every ending is one atomic decision: the paths that write (or succeed
    // writing nothing) have already committed. A failure settles it here, so
    // a stop request that arrives after the error is refused, and one that
    // arrived first reports Cancelled, never "Cancelling" then Failed.
    match result {
        Err(RunError::Failed(message)) => {
            if cancel.commit() {
                Err(RunError::Failed(message))
            } else {
                Err(RunError::Cancelled)
            }
        }
        other => other,
    }
}

/// [`Action::Text`]: the full pipeline, the ledger write `tpe extract` does,
/// and `<name>.txt` next to the source.
fn run_text(
    source: &Path,
    ledger: &Path,
    watch: &mut dyn FnMut(Progress) -> ControlFlow<()>,
    cancel: &CancelToken,
) -> Result<Outcome, RunError> {
    let job = EngineJob {
        path: source.to_string_lossy().into_owned(),
        backend: BACKEND.to_string(),
        pages: None,
        password: None,
        max_bytes: None,
        figures_dir: None,
    };
    let mut result =
        panic::catch_unwind(AssertUnwindSafe(|| pipeline::run_job_observed(&job, watch)))
            .unwrap_or_else(|payload| Err(panic_error(&*payload)))?;
    if result.status == Status::Failed {
        return Err(RunError::Failed(
            result
                .warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "extraction failed".to_string()),
        ));
    }
    // From here the job writes (ledger, then file): the last moment a stop
    // is accepted, and after it a stop request is refused.
    if !cancel.commit() {
        return Err(RunError::Cancelled);
    }
    if let Some(parent) = ledger.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let mut store = Ledger::open(ledger).map_err(|e| format!("ledger: {e}"))?;
    // As `tpe extract` does: the write is timed and recorded on the run.
    let write_start = Instant::now();
    let run = store
        .write_result(&result)
        .map_err(|e| format!("ledger: {e}"))?;
    result.timings.write_ms = write_start.elapsed().as_secs_f64() * 1000.0;
    store
        .update_timings(run, &result.timings)
        .map_err(|e| format!("ledger: {e}"))?;
    let texts: Vec<&str> = result.pages.iter().map(|p| p.text.as_str()).collect();
    let target = publish(source, &[(".txt", texts.join("\u{c}").into_bytes())])
        .map_err(|e| format!("writing next to {}: {e}", source.display()))?
        .remove(0);
    let mut summary = String::new();
    if result.status != Status::Complete {
        let _ = write!(summary, "{}: ", result.status.as_str());
    }
    let _ = write!(
        summary,
        "{}, {}",
        count(result.document.pages as usize, "page"),
        count(result.references.len(), "reference")
    );
    let warnings = if result.status == Status::Complete {
        Vec::new()
    } else {
        result.warnings
    };
    Ok(Outcome {
        outputs: vec![target],
        summary,
        warnings,
    })
}

/// [`Action::Bibliography`]: the backward scan and, when a list is found,
/// the CLI's JSON record plus its plain-text rendering next to the source.
fn run_bibliography(
    source: &Path,
    watch: &mut dyn FnMut(Progress) -> ControlFlow<()>,
    cancel: &CancelToken,
) -> Result<Outcome, RunError> {
    let started = Instant::now();
    let extractor = backend::by_name(BACKEND).ok_or("backend unavailable".to_string())?;
    let path_text = source.to_string_lossy();
    let (sha256, scan) = panic::catch_unwind(AssertUnwindSafe(|| {
        let snapshot = acquire::snapshot_polled(source, None, &mut |done, total| {
            watch(Progress::Reading { done, total })
        })
        .map_err(|error| match error {
            acquire::AcquireError::Stopped => RunError::Cancelled,
            other => RunError::Failed(other.to_string()),
        })?;
        let scan =
            bibliography::scan_backward_observed(extractor.as_ref(), &snapshot.bytes, None, watch)?;
        Ok::<_, RunError>((snapshot.hash.0, scan))
    }))
    .unwrap_or_else(|payload| Err(panic_error(&*payload).into()))?;
    let record = Record::from_scan(
        &path_text,
        sha256,
        extractor.identity(),
        scan,
        started.elapsed().as_secs_f64() * 1000.0,
    );
    if !record.found() {
        // Nothing is written, but the job still ends by the same atomic
        // decision: a stop asked for first wins (Cancelled); otherwise this
        // success is final and a later stop request is refused, so the row
        // never shows "Cancelling" for a job that then finishes.
        if !cancel.commit() {
            return Err(RunError::Cancelled);
        }
        return Ok(Outcome {
            outputs: Vec::new(),
            summary: "No reference list found".to_string(),
            warnings: record.warnings,
        });
    }
    let mut line = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    line.push('\n');
    let plain = record.plain_text();
    // Publication is the first write: the last moment a stop is accepted, and
    // after it a stop request is refused.
    if !cancel.commit() {
        return Err(RunError::Cancelled);
    }
    let outputs = publish(
        source,
        &[
            (".references.json", line.into_bytes()),
            (".references.txt", plain.into_bytes()),
        ],
    )
    .map_err(|e| format!("writing next to {}: {e}", source.display()))?;
    let scanned = record.pages_scanned.unwrap_or(0);
    let summary = format!(
        "{} from the last {}",
        count(record.references.len(), "reference"),
        count(scanned as usize, "page")
    );
    Ok(Outcome {
        outputs,
        summary,
        warnings: record.warnings,
    })
}

/// `1 page`, `2 pages`.
fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// A caught engine panic as the job's error.
fn panic_error(payload: &(dyn std::any::Any + Send)) -> pipeline::PipelineError {
    pipeline::PipelineError::UnknownBackend(format!("panic: {}", panic_message(payload)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Action, CancelToken, JobList, Mailbox, Outcome, Phase, RunError, Step, file_url_to_path,
        generation_paths, publish, run,
    };
    use futures::{FutureExt, StreamExt};
    use tpe::pipeline::Progress;

    /// A hand-written one-page PDF whose only text is "Just a note.".
    const NO_LIST_PDF: &[u8] = b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >> endobj\n4 0 obj << /Length 44 >> stream\nBT /F1 12 Tf 72 720 Td (Just a note.) Tj ET\nendstream endobj\n5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\ntrailer << /Root 1 0 R /Size 6 >>\n%%EOF\n";

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/synthetic-paper.pdf"
    );

    /// A fresh directory holding a copy of the fixture paper and a ledger path.
    fn scratch() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("paper.pdf");
        std::fs::copy(FIXTURE, &pdf).unwrap();
        let ledger = dir.path().join("state").join("ledger.sqlite");
        (dir, pdf, ledger)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn text_job_writes_sibling_text_with_progress() {
        let (dir, pdf, ledger) = scratch();
        let mut events = Vec::new();
        let outcome = run(
            Action::Text,
            &pdf,
            &ledger,
            &mut |e| {
                // The read of the file is not a page event.
                if !matches!(e, Progress::Reading { .. }) {
                    events.push(e);
                }
            },
            &CancelToken::new(),
        )
        .unwrap();

        assert_eq!(outcome.outputs, [dir.path().join("paper.txt")]);
        assert_eq!(outcome.summary, "2 pages, 3 references");
        assert!(outcome.warnings.is_empty());
        let text = std::fs::read_to_string(&outcome.outputs[0]).unwrap();
        assert!(text.contains("Faithful Extraction of Citations from Academic PDFs"));
        assert!(text.contains('\u{c}'), "pages are separated by a form feed");
        assert_eq!(
            events,
            [
                Progress::Opened { pages: 2, total: 2 },
                Progress::Page {
                    page: 1,
                    done: 1,
                    total: 2
                },
                Progress::Page {
                    page: 2,
                    done: 2,
                    total: 2
                },
            ]
        );
        assert!(ledger.exists(), "the ledger was created");
        let store = tpe::ledger::Ledger::open(&ledger).unwrap();
        let run_id = store
            .latest_run_for_prefix("")
            .unwrap()
            .expect("the run is in the ledger");
        let stored = store.load_result(run_id).unwrap();
        assert!(stored.timings.write_ms > 0.0, "the ledger write was timed");

        // A second run never overwrites: it numbers the new file.
        let again = run(
            Action::Text,
            &pdf,
            &ledger,
            &mut |_| {},
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(again.outputs, [dir.path().join("paper 2.txt")]);
    }

    #[test]
    fn bibliography_job_writes_json_and_text() {
        let (dir, pdf, ledger) = scratch();
        let mut events = Vec::new();
        let outcome = run(
            Action::Bibliography,
            &pdf,
            &ledger,
            &mut |e| {
                // The read of the file is not a page event.
                if !matches!(e, Progress::Reading { .. }) {
                    events.push(e);
                }
            },
            &CancelToken::new(),
        )
        .unwrap();

        assert_eq!(
            outcome.outputs,
            [
                dir.path().join("paper.references.json"),
                dir.path().join("paper.references.txt")
            ]
        );
        assert_eq!(outcome.summary, "3 references from the last 1 page");
        let json = std::fs::read_to_string(&outcome.outputs[0]).unwrap();
        let record: serde_json::Value = serde_json::from_str(json.trim_end()).unwrap();
        assert_eq!(record["status"], "found");
        assert_eq!(record["references"].as_array().unwrap().len(), 3);
        assert_eq!(record["sha256"].as_str().unwrap().len(), 64);
        let text = std::fs::read_to_string(&outcome.outputs[1]).unwrap();
        assert!(
            text.starts_with("[1] [1] A. Lovelace and C. Babbage"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 3);
        assert_eq!(
            events,
            [
                Progress::Opened { pages: 2, total: 2 },
                Progress::Page {
                    page: 2,
                    done: 1,
                    total: 2
                },
            ]
        );
        assert!(!ledger.exists(), "a bibliography needs no ledger");

        // With one file of the pair gone, the next run numbers both.
        std::fs::remove_file(&outcome.outputs[0]).unwrap();
        let again = run(
            Action::Bibliography,
            &pdf,
            &ledger,
            &mut |_| {},
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(
            again.outputs,
            [
                dir.path().join("paper 2.references.json"),
                dir.path().join("paper 2.references.txt")
            ]
        );
    }

    #[test]
    fn mailbox_keeps_items_until_a_sender_is_installed() {
        let mailbox: Mailbox<u32> = Mailbox::new();
        mailbox.send(1);
        mailbox.send(2);
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        mailbox.install(sender);
        mailbox.send(3);
        let (other, _keep) = futures::channel::mpsc::unbounded();
        mailbox.install(other); // ignored: the first sender stays
        mailbox.send(4);
        let mut got = Vec::new();
        while let Some(Some(item)) = receiver.next().now_or_never() {
            got.push(item);
        }
        assert_eq!(got, [1, 2, 3, 4]);
    }

    #[test]
    fn malformed_input_fails_and_writes_nothing() {
        let (dir, _pdf, ledger) = scratch();
        let junk = dir.path().join("junk.pdf");
        std::fs::write(&junk, b"not a pdf").unwrap();
        assert!(matches!(
            run(
                Action::Text,
                &junk,
                &ledger,
                &mut |_| {},
                &CancelToken::new()
            ),
            Err(RunError::Failed(_))
        ));
        assert!(matches!(
            run(
                Action::Bibliography,
                &junk,
                &ledger,
                &mut |_| {},
                &CancelToken::new()
            ),
            Err(RunError::Failed(_))
        ));
        assert_eq!(names(dir.path()), ["junk.pdf", "paper.pdf"]);
    }

    #[test]
    fn job_list_queues_one_at_a_time() {
        let (dir, pdf, _ledger) = scratch();
        let other = dir.path().join("other.PDF");
        std::fs::copy(&pdf, &other).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let mut list = JobList::default();
        let added = list.enqueue(
            [
                pdf.clone(),
                other.clone(),
                dir.path().join("notes.txt"),
                dir.path().join("missing.pdf"),
                dir.path().to_path_buf(),
            ],
            Action::Text,
        );
        assert_eq!(added, 2);
        assert_eq!(list.rows().len(), 2);
        assert_eq!(list.rows()[0].name(), "paper.pdf");
        assert_eq!(list.rows()[0].status_line(), "text · Waiting");

        let first = list.next_queued().unwrap();
        list.start(first);
        assert!(list.is_running());
        assert_eq!(list.next_queued(), None, "one job at a time");
        assert_eq!(list.row(first).unwrap().status_line(), "text · Opening");
        list.progress(first, Progress::Opened { pages: 4, total: 4 });
        list.progress(
            first,
            Progress::Page {
                page: 1,
                done: 1,
                total: 4,
            },
        );
        let row = list.row(first).unwrap();
        assert_eq!(row.status_line(), "text · Page 1 of 4");
        assert_eq!(row.fraction(), Some(0.25));
        assert!(!list.remove(first), "a running job is not removed");

        list.finish(
            first,
            Ok(Outcome {
                outputs: vec![dir.path().join("paper.txt")],
                summary: "2 pages, 3 references".into(),
                warnings: vec![],
            }),
        );
        list.progress(
            first,
            Progress::Page {
                page: 2,
                done: 2,
                total: 4,
            },
        );
        let row = list.row(first).unwrap();
        assert_eq!(row.done, 1, "late progress is ignored");
        assert_eq!(row.status_line(), "text · 2 pages, 3 references");
        assert_eq!(row.copyable(), Some(dir.path().join("paper.txt").as_path()));
        assert!(list.has_done());

        let second = list.next_queued().unwrap();
        assert!(list.remove(second), "a queued job is removed");
        assert_eq!(list.rows().len(), 1);
        list.clear_done();
        assert!(list.rows().is_empty());

        let mut list = JobList::default();
        list.enqueue([pdf], Action::Bibliography);
        let id = list.next_queued().unwrap();
        list.start(id);
        list.finish(id, Err(RunError::Failed("boom".into())));
        assert_eq!(list.rows()[0].status_line(), "bibliography · Failed: boom");
        assert_eq!(list.rows()[0].phase, Phase::Failed("boom".into()));
        assert_eq!(list.rows()[0].copyable(), None);
    }

    #[test]
    fn selection_steps_stop_at_the_ends() {
        let (dir, pdf, _ledger) = scratch();
        let mut list = JobList::default();
        assert_eq!(
            list.stepped(None, Step::Down),
            None,
            "an empty list selects nothing"
        );
        let paths: Vec<_> = (0..3)
            .map(|n| {
                let path = dir.path().join(format!("p{n}.pdf"));
                std::fs::copy(&pdf, &path).unwrap();
                path
            })
            .collect();
        list.enqueue(paths, Action::Text);
        let ids: Vec<usize> = list.rows().iter().map(|row| row.id).collect();

        assert_eq!(list.stepped(None, Step::Down), Some(ids[0]));
        assert_eq!(list.stepped(None, Step::Up), Some(ids[2]));
        assert_eq!(list.stepped(Some(ids[0]), Step::Down), Some(ids[1]));
        assert_eq!(list.stepped(Some(ids[2]), Step::Down), Some(ids[2]));
        assert_eq!(list.stepped(Some(ids[0]), Step::Up), Some(ids[0]));
        assert_eq!(list.stepped(Some(ids[1]), Step::First), Some(ids[0]));
        assert_eq!(list.stepped(Some(ids[1]), Step::Last), Some(ids[2]));
        assert_eq!(
            list.stepped(Some(999), Step::Down),
            Some(ids[0]),
            "a vanished row acts as none"
        );

        assert_eq!(list.index_of(ids[1]), Some(1));
        assert_eq!(list.nearest_to(1), Some(ids[1]));
        assert_eq!(list.nearest_to(10), Some(ids[2]));
    }

    #[test]
    fn generations_number_every_suffix_alike() {
        let source = Path::new("/docs/My Paper.v2.pdf");
        assert_eq!(
            generation_paths(source, &[".txt"], 1),
            [Path::new("/docs/My Paper.v2.txt")]
        );
        assert_eq!(
            generation_paths(source, &[".references.json", ".references.txt"], 3),
            [
                Path::new("/docs/My Paper.v2 3.references.json"),
                Path::new("/docs/My Paper.v2 3.references.txt")
            ]
        );
    }

    /// Names in `dir`, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        names(dir)
    }

    #[test]
    fn publish_never_overwrites_and_leaves_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("paper.pdf");
        std::fs::write(&source, b"pdf").unwrap();

        let first = publish(&source, &[(".txt", b"one".to_vec())]).unwrap();
        assert_eq!(first, [dir.path().join("paper.txt")]);
        let second = publish(&source, &[(".txt", b"two".to_vec())]).unwrap();
        assert_eq!(second, [dir.path().join("paper 2.txt")]);
        assert_eq!(
            std::fs::read(&first[0]).unwrap(),
            b"one",
            "the first file is untouched"
        );
        assert_eq!(std::fs::read(&second[0]).unwrap(), b"two");
        assert_eq!(
            listing(dir.path()),
            ["paper 2.txt", "paper.pdf", "paper.txt"]
        );
    }

    #[test]
    fn publish_moves_a_pair_on_together() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("paper.pdf");
        std::fs::write(&source, b"pdf").unwrap();
        std::fs::write(dir.path().join("paper.references.txt"), b"old").unwrap();

        let pair = publish(
            &source,
            &[
                (".references.json", b"{}".to_vec()),
                (".references.txt", b"new".to_vec()),
            ],
        )
        .unwrap();
        assert_eq!(
            pair,
            [
                dir.path().join("paper 2.references.json"),
                dir.path().join("paper 2.references.txt")
            ]
        );
        assert_eq!(
            std::fs::read(dir.path().join("paper.references.txt")).unwrap(),
            b"old"
        );
        assert_eq!(
            listing(dir.path()),
            [
                "paper 2.references.json",
                "paper 2.references.txt",
                "paper.pdf",
                "paper.references.txt"
            ],
            "nothing half-made is left: the failed first attempt was undone"
        );
    }

    #[test]
    fn concurrent_publishers_each_get_their_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("paper.pdf");
        std::fs::write(&source, b"pdf").unwrap();
        let written: Vec<(usize, std::path::PathBuf)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|n| {
                    let source = &source;
                    scope.spawn(move || {
                        let paths =
                            publish(source, &[(".txt", format!("writer {n}").into_bytes())])
                                .unwrap();
                        (n, paths.into_iter().next().unwrap())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let mut distinct: Vec<_> = written.iter().map(|(_, path)| path.clone()).collect();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 8, "no two publishers took the same name");
        for (n, path) in &written {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                format!("writer {n}")
            );
        }
        assert_eq!(
            listing(dir.path()).len(),
            9,
            "the source and eight outputs, no temporaries"
        );
    }

    #[test]
    fn a_stop_requested_at_the_first_page_ends_a_text_job_with_nothing_written() {
        let (dir, pdf, ledger) = scratch();
        let cancel = CancelToken::new();
        let mut seen = Vec::new();
        let result = run(
            Action::Text,
            &pdf,
            &ledger,
            &mut |event| {
                if !matches!(event, Progress::Reading { .. }) {
                    seen.push(event);
                }
                if matches!(event, Progress::Page { .. }) {
                    cancel.request();
                }
            },
            &cancel,
        );
        assert_eq!(result, Err(RunError::Cancelled));
        assert_eq!(
            seen.len(),
            2,
            "the open and page 1 were reported; page 2 was never read: {seen:?}"
        );
        assert_eq!(
            listing(dir.path()),
            ["paper.pdf"],
            "no output and no ledger directory"
        );
        assert!(!ledger.exists());
    }

    #[test]
    fn a_stop_before_the_start_ends_either_job_kind_at_once() {
        let (dir, pdf, ledger) = scratch();
        let cancel = CancelToken::new();
        assert!(cancel.request());
        for action in [Action::Text, Action::Bibliography] {
            let mut events = 0;
            let result = run(action, &pdf, &ledger, &mut |_| events += 1, &cancel);
            assert_eq!(result, Err(RunError::Cancelled), "{action:?}");
            assert_eq!(events, 0, "the file was not even opened");
        }
        assert_eq!(listing(dir.path()), ["paper.pdf"]);
    }

    #[test]
    fn a_stop_while_the_file_is_read_ends_either_job_kind_with_nothing_written() {
        for action in [Action::Text, Action::Bibliography] {
            let (dir, pdf, ledger) = scratch();
            let cancel = CancelToken::new();
            let mut pages = 0;
            let result = run(
                action,
                &pdf,
                &ledger,
                &mut |event| match event {
                    Progress::Reading { .. } => {
                        cancel.request();
                    }
                    _ => pages += 1,
                },
                &cancel,
            );
            assert_eq!(result, Err(RunError::Cancelled), "{action:?}");
            assert_eq!(pages, 0, "{action:?}: the document was never opened");
            assert_eq!(listing(dir.path()), ["paper.pdf"], "{action:?}");
        }
    }

    #[test]
    fn a_stopped_bibliography_scan_writes_nothing() {
        let (dir, pdf, ledger) = scratch();
        let cancel = CancelToken::new();
        let result = run(
            Action::Bibliography,
            &pdf,
            &ledger,
            &mut |event| {
                if matches!(event, Progress::Page { .. }) {
                    cancel.request();
                }
            },
            &cancel,
        );
        assert_eq!(result, Err(RunError::Cancelled));
        assert_eq!(listing(dir.path()), ["paper.pdf"]);
    }

    #[test]
    fn a_stop_is_accepted_only_until_the_job_starts_writing() {
        let stop_first = CancelToken::new();
        assert!(stop_first.request());
        assert!(stop_first.request(), "asking again is still a stop");
        assert!(stop_first.is_requested());
        assert!(
            !stop_first.commit(),
            "a stop asked for first wins: no writes"
        );

        let write_first = CancelToken::new();
        assert!(write_first.commit());
        assert!(!write_first.request(), "too late: it is writing");
        assert!(!write_first.is_requested());
        assert!(write_first.commit(), "committing twice is harmless");
    }

    #[test]
    fn a_finished_job_refuses_a_late_stop_and_wrote_its_files() {
        let (dir, pdf, ledger) = scratch();
        for action in [Action::Text, Action::Bibliography] {
            let cancel = CancelToken::new();
            let outcome = run(action, &pdf, &ledger, &mut |_| {}, &cancel).unwrap();
            assert!(!cancel.request(), "{action:?}: the job had committed");
            assert!(
                outcome.outputs.iter().all(|path| path.is_file()),
                "{action:?}"
            );
        }
        assert!(dir.path().join("paper.txt").is_file());
    }

    #[test]
    fn a_failed_job_settles_the_stop_decision_too() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.pdf");
        std::fs::write(&junk, b"not a pdf").unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        for action in [Action::Text, Action::Bibliography] {
            let cancel = CancelToken::new();
            let result = run(action, &junk, &ledger, &mut |_| {}, &cancel);
            assert!(matches!(result, Err(RunError::Failed(_))), "{action:?}");
            assert!(
                !cancel.request(),
                "{action:?}: the failure was final, a later stop is refused"
            );
        }
    }

    #[test]
    fn a_no_list_result_also_refuses_a_late_stop() {
        // A one-line PDF with no reference list: the job succeeds writing
        // nothing, and must still end by the same atomic decision.
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("note.pdf");
        std::fs::write(&pdf, NO_LIST_PDF).unwrap();
        let ledger = dir.path().join("ledger.sqlite");

        let cancel = CancelToken::new();
        let outcome = run(Action::Bibliography, &pdf, &ledger, &mut |_| {}, &cancel).unwrap();
        assert!(outcome.outputs.is_empty(), "{outcome:?}");
        assert_eq!(outcome.summary, "No reference list found");
        assert!(!cancel.request(), "the success was final");

        // A stop that wins the race cancels it instead of finishing.
        let stopped = CancelToken::new();
        let result = run(
            Action::Bibliography,
            &pdf,
            &ledger,
            &mut |event| {
                if matches!(event, Progress::Page { .. }) {
                    stopped.request();
                }
            },
            &stopped,
        );
        assert_eq!(result, Err(RunError::Cancelled));
    }

    #[test]
    fn a_clash_retries_the_links_not_the_writes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("paper.pdf");
        std::fs::write(&source, b"pdf").unwrap();
        for name in ["paper.txt", "paper 2.txt", "paper 3.txt"] {
            std::fs::write(dir.path().join(name), b"old").unwrap();
        }
        let staged = super::stage(&source, &[(".txt", b"new".to_vec())]).unwrap();
        let partials = |dir: &Path| {
            listing(dir)
                .into_iter()
                .filter(|name| name.ends_with(".partial"))
                .count()
        };
        assert_eq!(partials(dir.path()), 1, "written once");
        for generation in 1..=3 {
            let finals = generation_paths(&source, &[".txt"], generation);
            let error = super::link_all(&staged.0, &finals).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
            assert_eq!(partials(dir.path()), 1, "no second copy staged");
        }
        let free = generation_paths(&source, &[".txt"], 4);
        super::link_all(&staged.0, &free).unwrap();
        assert_eq!(std::fs::read(&free[0]).unwrap(), b"new");
        drop(staged);
        assert_eq!(partials(dir.path()), 0, "the temporary name is gone");
        assert_eq!(std::fs::read(&free[0]).unwrap(), b"new", "the link remains");
    }

    #[test]
    fn a_stale_staging_name_from_a_killed_run_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("paper.pdf");
        std::fs::write(&source, b"pdf").unwrap();
        // What a force-quit leaves: the names the next few stagings would take
        // (this process's id, counters from wherever the counter is now).
        let now = super::STAGING.load(std::sync::atomic::Ordering::Relaxed);
        for n in now..now + 40 {
            std::fs::write(
                dir.path().join(format!(
                    ".pdftextract-{}-{n}.txt.partial",
                    std::process::id()
                )),
                b"stale",
            )
            .unwrap();
        }
        let published = publish(&source, &[(".txt", b"fresh".to_vec())]).unwrap();
        assert_eq!(std::fs::read(&published[0]).unwrap(), b"fresh");
    }

    #[test]
    fn only_a_missing_hard_link_facility_falls_back_to_writing_in_place() {
        use std::io::{Error, ErrorKind};
        // Kinds, not errno numbers, which differ between Linux and macOS.
        for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
            assert!(super::links_unsupported(&Error::from(kind)), "{kind:?}");
        }
        // Real failures are returned as they are, never retried by writing.
        for kind in [
            ErrorKind::AlreadyExists,
            ErrorKind::StorageFull,
            ErrorKind::QuotaExceeded,
            ErrorKind::Other,
        ] {
            assert!(!super::links_unsupported(&Error::from(kind)), "{kind:?}");
        }
        #[cfg(target_os = "macos")]
        assert!(
            super::links_unsupported(&Error::from_raw_os_error(45)),
            "ENOTSUP"
        );
    }

    #[test]
    fn cancelling_is_shown_then_settles_as_cancelled() {
        let (_dir, pdf, _ledger) = scratch();
        let mut list = JobList::default();
        list.enqueue([pdf], Action::Text);
        let id = list.next_queued().unwrap();
        assert!(
            !list.mark_cancelling(id),
            "a queued row is removed, not marked"
        );
        list.start(id);
        assert!(list.mark_cancelling(id));
        assert_eq!(list.row(id).unwrap().status_line(), "text · Cancelling");
        assert!(list.row(id).unwrap().is_active());
        list.finish(id, Err(RunError::Cancelled));
        let row = list.row(id).unwrap();
        assert_eq!(row.phase, Phase::Cancelled);
        assert_eq!(row.status_line(), "text · Cancelled");
        assert!(
            !row.is_active(),
            "a cancelled row is done and can be cleared"
        );
        assert!(!list.mark_cancelling(id));
    }

    #[test]
    fn file_urls_become_paths() {
        assert_eq!(
            file_url_to_path("file:///Users/ben/My%20Paper%20%2B%20notes.pdf"),
            Some("/Users/ben/My Paper + notes.pdf".into())
        );
        assert_eq!(
            file_url_to_path("file://localhost/tmp/a.pdf"),
            Some("/tmp/a.pdf".into())
        );
        assert_eq!(file_url_to_path("https://example.org/a.pdf"), None);
        assert_eq!(file_url_to_path("file://host/a.pdf"), None);
        assert_eq!(file_url_to_path("file:///bad%zz.pdf"), None);
    }
}
