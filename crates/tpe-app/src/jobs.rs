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
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use futures::channel::mpsc::UnboundedSender;

use crate::publication::StagedOutputs;

use tpe::acquire;
use tpe::backend;
use tpe::bibliography::{self, Record};
use tpe::ledger::Ledger;
use tpe::pipeline::{self, Progress};
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
            Phase::Running => match self.total {
                Some(total) => format!("Page {} of {total}", self.done),
                None => "Opening".to_string(),
            },
            Phase::Finished(outcome) => match outcome.warnings.first() {
                Some(warning) => format!("{} · {warning}", outcome.summary),
                None => outcome.summary.clone(),
            },
            Phase::Failed(message) => format!("Failed: {message}"),
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
        }
    }

    pub fn finish(&mut self, id: usize, result: Result<Outcome, String>) {
        if let Some(row) = self.row_mut(id) {
            row.phase = match result {
                Ok(outcome) => Phase::Finished(outcome),
                Err(message) => Phase::Failed(message),
            };
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

    /// Whether any row is queued or running.
    pub fn has_active(&self) -> bool {
        self.rows.iter().any(JobRow::is_active)
    }

    /// Drop finished and failed rows.
    pub fn clear_done(&mut self) {
        self.rows.retain(JobRow::is_active);
    }
}

/// Where an output file goes: next to the source PDF, with the source's base
/// name plus `suffix`, never overwriting an existing file. `paper.pdf` with
/// suffix `.txt` becomes `paper.txt`, then `paper 2.txt`, `paper 3.txt`, …
pub fn output_path(source: &Path, suffix: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    output_paths(source, &[suffix], exists).remove(0)
}

/// [`output_path`] for a set of files written together: they share one
/// generation number, chosen so that none of them exists. With
/// `paper.references.txt` present but `paper.references.json` gone, both
/// become `paper 2.references.*` rather than a pair mixing two runs.
pub fn output_paths(
    source: &Path,
    suffixes: &[&str],
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let directory = source.parent().unwrap_or_else(|| Path::new(""));
    let base = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let candidates = |n: u64| -> Vec<PathBuf> {
        suffixes
            .iter()
            .map(|suffix| {
                if n == 1 {
                    directory.join(format!("{base}{suffix}"))
                } else {
                    directory.join(format!("{base} {n}{suffix}"))
                }
            })
            .collect()
    };
    let mut n = 1u64;
    let mut paths = candidates(n);
    while paths.iter().any(|path| exists(path)) {
        n += 1;
        paths = candidates(n);
    }
    paths
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

/// Run one job in this process. `observe` is called on this thread for the
/// open and for each page. Outputs are written next to `source` only on
/// success. Outputs are staged before touching the ledger; handled failures
/// roll back the result transaction and remove outputs created by this job.
/// Cleanup errors name retained paths. See docs/PUBLICATION.md for crash semantics.
pub fn run(
    action: Action,
    source: &Path,
    ledger: &Path,
    observe: &mut dyn FnMut(Progress),
) -> Result<Outcome, String> {
    match action {
        Action::Text => run_text(source, ledger, observe),
        Action::Bibliography => run_bibliography(source, observe),
    }
}

/// [`Action::Text`]: the full pipeline, the ledger write `tpe extract` does,
/// and `<name>.txt` next to the source.
fn run_text(
    source: &Path,
    ledger: &Path,
    observe: &mut dyn FnMut(Progress),
) -> Result<Outcome, String> {
    let job = EngineJob {
        path: source.to_string_lossy().into_owned(),
        backend: BACKEND.to_string(),
        pages: None,
        password: None,
        max_bytes: None,
        figures_dir: None,
    };
    let mut result = panic::catch_unwind(AssertUnwindSafe(|| {
        pipeline::run_job_observed(&job, observe)
    }))
    .unwrap_or_else(|payload| Err(panic_error(&*payload)))
    .map_err(|err| err.to_string())?;
    if result.status == Status::Failed {
        return Err(result
            .warnings
            .first()
            .cloned()
            .unwrap_or_else(|| "extraction failed".to_string()));
    }
    let texts: Vec<&str> = result.pages.iter().map(|p| p.text.as_str()).collect();
    let mut outputs = StagedOutputs::stage(source, &[(".txt", texts.join("\u{c}").into_bytes())])
        .map_err(|e| format!("staging output: {e}"))?;
    if let Some(parent) = ledger.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let mut store = Ledger::open(ledger).map_err(|e| format!("ledger: {e}"))?;
    let write_start = Instant::now();
    let pending = store
        .prepare_result(&result)
        .map_err(|e| format!("ledger: {e}"))?;
    result.timings.write_ms = write_start.elapsed().as_secs_f64() * 1000.0;
    pending
        .update_timings(&result.timings)
        .map_err(|e| format!("ledger: {e}"))?;
    // On a publication failure `pending` drops and restores the prior run.
    // On a commit failure the helper removes its own published links.
    let paths = outputs.publish_then(|| {
        pending
            .commit()
            .map(|_| ())
            .map_err(|e| format!("ledger commit: {e}"))
    })?;
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
        outputs: paths,
        summary,
        warnings,
    })
}

/// [`Action::Bibliography`]: the backward scan and, when a list is found,
/// the CLI's JSON record plus its plain-text rendering next to the source.
fn run_bibliography(source: &Path, observe: &mut dyn FnMut(Progress)) -> Result<Outcome, String> {
    let started = Instant::now();
    let extractor = backend::by_name(BACKEND).ok_or("backend unavailable")?;
    let path_text = source.to_string_lossy();
    let (sha256, scan) = panic::catch_unwind(AssertUnwindSafe(|| {
        let snapshot = acquire::snapshot(source, None).map_err(|e| e.to_string())?;
        let scan = bibliography::scan_backward_observed(
            extractor.as_ref(),
            &snapshot.bytes,
            None,
            observe,
        )
        .map_err(|e| e.to_string())?;
        Ok::<_, String>((snapshot.hash.0, scan))
    }))
    .unwrap_or_else(|payload| Err(panic_error(&*payload).to_string()))?;
    let record = Record::from_scan(
        &path_text,
        sha256,
        extractor.identity(),
        scan,
        started.elapsed().as_secs_f64() * 1000.0,
    );
    if !record.found() {
        return Ok(Outcome {
            outputs: Vec::new(),
            summary: "No reference list found".to_string(),
            warnings: record.warnings,
        });
    }
    let mut line = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    line.push('\n');
    let mut staged = StagedOutputs::stage(
        source,
        &[
            (".references.json", line.into_bytes()),
            (".references.txt", record.plain_text().into_bytes()),
        ],
    )
    .map_err(|e| format!("staging bibliography: {e}"))?;
    let outputs = staged.publish_then(|| Ok(()))?;
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
        Action, JobList, Mailbox, Outcome, Phase, file_url_to_path, output_path, output_paths, run,
    };
    use futures::{FutureExt, StreamExt};
    use tpe::pipeline::Progress;

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
        let outcome = run(Action::Text, &pdf, &ledger, &mut |e| events.push(e)).unwrap();

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
        let again = run(Action::Text, &pdf, &ledger, &mut |_| {}).unwrap();
        assert_eq!(again.outputs, [dir.path().join("paper 2.txt")]);
    }

    #[test]
    fn ledger_open_failure_leaves_no_final_or_staged_output() {
        let (dir, pdf, ledger) = scratch();
        std::fs::create_dir_all(&ledger).unwrap(); // a directory cannot be a SQLite file
        let error = run(Action::Text, &pdf, &ledger, &mut |_| {}).unwrap_err();
        assert!(error.contains("ledger:"), "{error}");
        assert!(!dir.path().join("paper.txt").exists());
        assert!(
            !names(dir.path())
                .iter()
                .any(|name| name.starts_with(".pdftextract-"))
        );
    }

    #[test]
    fn bibliography_job_writes_json_and_text() {
        let (dir, pdf, ledger) = scratch();
        let mut events = Vec::new();
        let outcome = run(Action::Bibliography, &pdf, &ledger, &mut |e| events.push(e)).unwrap();

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
        let again = run(Action::Bibliography, &pdf, &ledger, &mut |_| {}).unwrap();
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
        assert!(run(Action::Text, &junk, &ledger, &mut |_| {}).is_err());
        assert!(run(Action::Bibliography, &junk, &ledger, &mut |_| {}).is_err());
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
        list.finish(id, Err("boom".into()));
        assert_eq!(list.rows()[0].status_line(), "bibliography · Failed: boom");
        assert_eq!(list.rows()[0].phase, Phase::Failed("boom".into()));
        assert_eq!(list.rows()[0].copyable(), None);
    }

    #[test]
    fn output_names_avoid_existing_files() {
        let source = Path::new("/docs/My Paper.v2.pdf");
        let taken = ["/docs/My Paper.v2.txt", "/docs/My Paper.v2 2.txt"];
        let exists = |p: &Path| taken.contains(&p.to_str().unwrap());
        assert_eq!(
            output_path(source, ".txt", |_| false),
            Path::new("/docs/My Paper.v2.txt")
        );
        assert_eq!(
            output_path(source, ".txt", exists),
            Path::new("/docs/My Paper.v2 3.txt")
        );
        assert_eq!(
            output_path(source, ".references.json", |_| false),
            Path::new("/docs/My Paper.v2.references.json")
        );
        // A set shares one generation: any member present moves them all on.
        let only_txt = |p: &Path| p.to_str().unwrap() == "/docs/My Paper.v2.references.txt";
        assert_eq!(
            output_paths(source, &[".references.json", ".references.txt"], only_txt),
            [
                Path::new("/docs/My Paper.v2 2.references.json"),
                Path::new("/docs/My Paper.v2 2.references.txt")
            ]
        );
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
