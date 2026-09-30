//! The server's jobs: the app's [`JobList`] (one row per PDF and action, one
//! job running at a time) plus what the API adds per row, and the engine
//! thread that runs them with the app's [`jobs::run`].
//!
//! Handlers and the engine thread share one mutex; every critical section is
//! a few field updates or one JSON serialisation, never engine work. Each
//! change to a job bumps a global sequence number, stores it on the job and
//! publishes it on the job's `watch` channel: a one-slot channel that keeps
//! only the newest value, so an event stream (`sse.rs`) that falls behind is
//! woken once and sends the job's current state, never a backlog. That is the
//! app's own coalescing (keep only the newest progress event per frame).

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use http::StatusCode;
use serde::Serialize;
use serde::ser::{SerializeSeq as _, SerializeStruct as _, Serializer};
use tokio::sync::watch;
use tpe::pipeline::Progress;
use tpe_app::jobs::{self, Action, CancelToken, JobList, JobRow, Outcome, Phase, RunError};

use crate::error::{ApiError, PathIssue};

/// Device and inode of a file the server wrote, taken right after the job
/// wrote it: the output endpoint serves the file only while the name still
/// refers to that same file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    pub fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

/// What the API keeps beside each [`JobRow`].
struct Meta {
    /// The path as the client sent it (echoed back; never the resolved one).
    sent: String,
    /// The resolved path the engine reads.
    source: PathBuf,
    /// Sequence number of this job's latest change (the SSE event id).
    seq: u64,
    /// The last engine progress event.
    last: Option<Progress>,
    notify: watch::Sender<u64>,
    /// One per output, `None` if it could not be examined after writing.
    outputs: Vec<Option<FileId>>,
    /// The `Idempotency-Key` of the batch that made it.
    key: Option<String>,
}

/// A batch submitted with an `Idempotency-Key`.
struct Batch {
    action: Action,
    paths: Vec<String>,
    ids: Vec<usize>,
}

struct Inner {
    list: JobList,
    meta: HashMap<usize, Meta>,
    seq: u64,
    batches: HashMap<String, Batch>,
    running: Option<(usize, Arc<CancelToken>)>,
    stopping: bool,
}

impl Inner {
    /// Record a change to job `id` and wake its event streams.
    fn touch(&mut self, id: usize) {
        self.seq += 1;
        if let Some(meta) = self.meta.get_mut(&id) {
            meta.seq = self.seq;
            meta.notify.send_replace(self.seq);
        }
    }

    fn view<'a>(&'a self, instance: &'a str, id: usize) -> Option<JobView<'a>> {
        Some(JobView {
            instance,
            row: self.list.row(id)?,
            meta: self.meta.get(&id)?,
        })
    }

    /// `{"jobs":[…]}` for `ids` that still exist.
    fn jobs_json(&self, instance: &str, ids: &[usize]) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + ids.len() * 320);
        out.extend_from_slice(b"{\"jobs\":[");
        let mut first = true;
        for id in ids {
            if let Some(view) = self.view(instance, *id) {
                if !first {
                    out.push(b',');
                }
                first = false;
                let _ = serde_json::to_writer(&mut out, &view);
            }
        }
        out.extend_from_slice(b"]}");
        out
    }
}

/// The result of asking for a job's state after event `after`.
pub enum Frame {
    /// The job was deleted.
    Removed,
    /// Nothing newer than `after`.
    Unchanged { terminal: bool },
    Newer {
        seq: u64,
        json: Vec<u8>,
        terminal: bool,
    },
}

pub struct Store {
    inner: Mutex<Inner>,
    /// Signalled when a job is queued or the server stops.
    work: Condvar,
    instance: String,
    ledger: PathBuf,
    state_dir: PathBuf,
    max_jobs: usize,
}

impl Store {
    /// `instance` prefixes every job id, so an id from an earlier run of the
    /// server is unknown to this one rather than naming another job.
    pub fn new(instance: String, ledger: PathBuf, max_jobs: usize) -> Self {
        let state_dir = ledger.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            inner: Mutex::new(Inner {
                list: JobList::default(),
                meta: HashMap::new(),
                seq: 0,
                batches: HashMap::new(),
                running: None,
                stopping: false,
            }),
            work: Condvar::new(),
            instance,
            ledger,
            state_dir,
            max_jobs,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The row id in a job id `<instance>-<n>`.
    pub fn parse_id(&self, text: &str) -> Option<usize> {
        let (instance, number) = text.split_once('-')?;
        if instance != self.instance
            || number.is_empty()
            || !number.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        number.parse().ok()
    }

    /// Queue one job per path, all or none. `sent` are the paths as the
    /// client wrote them, `sources` their resolved forms. With `key`, a
    /// repeat of the same batch returns the jobs it made the first time
    /// (`true`) instead of queueing them again.
    pub fn submit(
        &self,
        action: Action,
        sent: Vec<String>,
        sources: Vec<PathBuf>,
        key: Option<String>,
    ) -> Result<(bool, Vec<u8>), ApiError> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if inner.stopping {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "the server is stopping",
            ));
        }
        if let Some(batch) = key.as_ref().and_then(|k| inner.batches.get(k)) {
            if batch.action != action || batch.paths != sent {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "idempotency_key_reused",
                    "this Idempotency-Key was used for a different batch",
                ));
            }
            return Ok((true, inner.jobs_json(&self.instance, &batch.ids)));
        }
        if inner.list.rows().len() + sources.len() > self.max_jobs {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "job_limit",
                format!(
                    "the server keeps at most {} jobs; delete finished ones first",
                    self.max_jobs
                ),
            ));
        }
        let mut ids = Vec::with_capacity(sources.len());
        for (index, source) in sources.into_iter().enumerate() {
            // `JobList::enqueue` checks the file again; one that vanished
            // since `paths::validate` undoes the whole batch.
            if inner.list.enqueue([source.clone()], action) != 1 {
                for id in &ids {
                    inner.list.remove(*id);
                    inner.meta.remove(id);
                }
                let mut error = ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_path",
                    "a path was refused; nothing was queued",
                );
                error.details.push(PathIssue {
                    index,
                    reason: "not_found",
                });
                return Err(error);
            }
            let Some(id) = inner.list.rows().last().map(|row| row.id) else {
                return Err(ApiError::internal("the job list lost a row"));
            };
            let (notify, _) = watch::channel(0);
            inner.meta.insert(
                id,
                Meta {
                    sent: sent[index].clone(),
                    source,
                    seq: 0,
                    last: None,
                    notify,
                    outputs: Vec::new(),
                    key: key.clone(),
                },
            );
            inner.touch(id);
            ids.push(id);
        }
        let body = inner.jobs_json(&self.instance, &ids);
        for id in &ids {
            log(format_args!(
                "job {}: queued ({})",
                JobId(&self.instance, *id),
                action.noun()
            ));
        }
        if let Some(key) = key {
            inner.batches.insert(
                key,
                Batch {
                    action,
                    paths: sent,
                    ids,
                },
            );
        }
        drop(guard);
        self.work.notify_one();
        Ok((false, body))
    }

    /// `{"jobs":[…]}`, every job in arrival order.
    pub fn list_json(&self) -> Vec<u8> {
        let inner = self.lock();
        let rows = inner.list.rows();
        let mut out = Vec::with_capacity(16 + rows.len() * 320);
        out.extend_from_slice(b"{\"jobs\":[");
        let mut first = true;
        for row in rows {
            let Some(meta) = inner.meta.get(&row.id) else {
                continue;
            };
            if !first {
                out.push(b',');
            }
            first = false;
            let view = JobView {
                instance: &self.instance,
                row,
                meta,
            };
            let _ = serde_json::to_writer(&mut out, &view);
        }
        out.extend_from_slice(b"]}");
        out
    }

    pub fn job_json(&self, id: usize) -> Result<Vec<u8>, ApiError> {
        let inner = self.lock();
        let view = inner
            .view(&self.instance, id)
            .ok_or_else(ApiError::job_not_found)?;
        serde_json::to_vec(&view).map_err(|_| ApiError::internal("serialising a job failed"))
    }

    /// Stop job `id`. A queued job is cancelled at once; a running one stops
    /// at its next page unless it has started writing its results
    /// ([`CancelToken`]), in which case the stop is refused and the job
    /// finishes. Returns `{"accepted":true,"job":…}`.
    pub fn cancel(&self, id: usize) -> Result<Vec<u8>, ApiError> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let row = inner.list.row(id).ok_or_else(ApiError::job_not_found)?;
        match row.phase {
            Phase::Queued => {
                inner.list.finish(id, Err(RunError::Cancelled));
                inner.touch(id);
                log(format_args!(
                    "job {}: cancelled while queued",
                    JobId(&self.instance, id)
                ));
            }
            Phase::Running => {
                let was_cancelling = row.cancelling;
                let accepted = matches!(
                    &inner.running,
                    Some((running, token)) if *running == id && token.request()
                );
                if !accepted {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "cancel_too_late",
                        "the job is already writing its results; it will finish",
                    ));
                }
                if !was_cancelling && inner.list.mark_cancelling(id) {
                    inner.touch(id);
                    log(format_args!(
                        "job {}: cancelling",
                        JobId(&self.instance, id)
                    ));
                }
            }
            Phase::Cancelled => {}
            Phase::Finished(_) | Phase::Failed(_) => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "already_finished",
                    "the job has already finished",
                ));
            }
        }
        let mut out = b"{\"accepted\":true,\"job\":".to_vec();
        if let Some(view) = inner.view(&self.instance, id) {
            let _ = serde_json::to_writer(&mut out, &view);
        }
        out.push(b'}');
        Ok(out)
    }

    /// Forget job `id` if it is queued or done. Its output files stay where
    /// they are. A running job must be cancelled first.
    pub fn delete(&self, id: usize) -> Result<(), ApiError> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let row = inner.list.row(id).ok_or_else(ApiError::job_not_found)?;
        let removed = match row.phase {
            Phase::Running => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "job_running",
                    "the job is running; cancel it first",
                ));
            }
            Phase::Queued => inner.list.remove(id),
            _ => inner.list.remove_done(id),
        };
        if !removed {
            return Err(ApiError::internal("the job list kept a row"));
        }
        // Dropping the metadata drops the job's watch sender, which ends its
        // event streams.
        if let Some(key) = inner.meta.remove(&id).and_then(|meta| meta.key) {
            let spent = inner
                .batches
                .get(&key)
                .is_some_and(|batch| batch.ids.iter().all(|i| !inner.meta.contains_key(i)));
            if spent {
                inner.batches.remove(&key);
            }
        }
        Ok(())
    }

    /// A receiver that wakes on every change to job `id`.
    pub fn subscribe(&self, id: usize) -> Option<watch::Receiver<u64>> {
        self.lock()
            .meta
            .get(&id)
            .map(|meta| meta.notify.subscribe())
    }

    /// The job's state, if it changed after event `after`.
    pub fn frame(&self, id: usize, after: u64) -> Frame {
        let inner = self.lock();
        let Some(view) = inner.view(&self.instance, id) else {
            return Frame::Removed;
        };
        let terminal = !view.row.is_active();
        if view.meta.seq <= after {
            return Frame::Unchanged { terminal };
        }
        Frame::Newer {
            seq: view.meta.seq,
            json: serde_json::to_vec(&view).unwrap_or_default(),
            terminal,
        }
    }

    /// Output `n` of finished job `id`: the path the job wrote and the file
    /// it wrote there.
    pub fn output(&self, id: usize, n: usize) -> Result<(PathBuf, Option<FileId>), ApiError> {
        let inner = self.lock();
        let row = inner.list.row(id).ok_or_else(ApiError::job_not_found)?;
        let no_output = || ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such output");
        match &row.phase {
            Phase::Finished(outcome) => {
                let path = outcome.outputs.get(n).ok_or_else(no_output)?;
                let identity = inner
                    .meta
                    .get(&id)
                    .and_then(|meta| meta.outputs.get(n).copied().flatten());
                Ok((path.clone(), identity))
            }
            Phase::Queued | Phase::Running => Err(ApiError::new(
                StatusCode::CONFLICT,
                "not_finished",
                "the job has not finished",
            )),
            Phase::Failed(_) | Phase::Cancelled => Err(no_output()),
        }
    }

    /// The engine thread: run queued jobs one at a time until [`Store::stop`].
    pub fn work(&self) {
        while let Some((id, action, source, cancel)) = self.next() {
            let result = jobs::run(
                action,
                &source,
                &self.ledger,
                &mut |event| self.progress(id, event),
                &cancel,
            );
            let identities = match &result {
                Ok(outcome) => outcome
                    .outputs
                    .iter()
                    .map(|path| fs::symlink_metadata(path).ok().map(|m| FileId::of(&m)))
                    .collect(),
                Err(_) => Vec::new(),
            };
            self.finish(id, result, identities);
        }
    }

    /// Wait for the oldest queued job and mark it running.
    fn next(&self) -> Option<(usize, Action, PathBuf, Arc<CancelToken>)> {
        let mut inner = self.lock();
        loop {
            if inner.stopping {
                return None;
            }
            if let Some(id) = inner.list.next_queued() {
                let action = inner.list.row(id).map(|row| row.action);
                let source = inner.meta.get(&id).map(|meta| meta.source.clone());
                let (Some(action), Some(source)) = (action, source) else {
                    inner.list.start(id);
                    inner
                        .list
                        .finish(id, Err(RunError::Failed("internal: job state lost".into())));
                    continue;
                };
                inner.list.start(id);
                let cancel = Arc::new(CancelToken::new());
                inner.running = Some((id, cancel.clone()));
                inner.touch(id);
                return Some((id, action, source, cancel));
            }
            inner = self
                .work
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn progress(&self, id: usize, event: Progress) {
        let mut inner = self.lock();
        inner.list.progress(id, event);
        if let Some(meta) = inner.meta.get_mut(&id) {
            meta.last = Some(event);
        }
        inner.touch(id);
    }

    fn finish(
        &self,
        id: usize,
        result: Result<Outcome, RunError>,
        identities: Vec<Option<FileId>>,
    ) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let result = match (result, inner.meta.get(&id)) {
            (Err(RunError::Failed(message)), Some(meta)) => Err(RunError::Failed(redact(
                &message,
                &meta.source,
                &meta.sent,
                &self.state_dir,
            ))),
            (other, _) => other,
        };
        let state = match &result {
            Ok(_) => "finished",
            Err(RunError::Cancelled) => "cancelled",
            Err(RunError::Failed(_)) => "failed",
        };
        inner.list.finish(id, result);
        if let Some(meta) = inner.meta.get_mut(&id) {
            meta.outputs = identities;
        }
        inner.running = None;
        inner.touch(id);
        log(format_args!("job {}: {state}", JobId(&self.instance, id)));
    }

    /// Stop taking jobs, ask the running one to stop, and wake the engine
    /// thread so it returns.
    pub fn stop(&self) {
        let mut inner = self.lock();
        inner.stopping = true;
        if let Some((_, token)) = &inner.running {
            token.request();
        }
        drop(inner);
        self.work.notify_all();
    }
}

/// Replace the resolved source path and the state directory in an engine
/// message with what the client sent, so a failure never names a path the
/// client did not.
fn redact(message: &str, source: &Path, sent: &str, state_dir: &Path) -> String {
    let mut out = message.replace(source.to_string_lossy().as_ref(), sent);
    let state = state_dir.to_string_lossy();
    if !state.is_empty() {
        out = out.replace(state.as_ref(), "<state directory>");
    }
    out
}

/// Logs go to stderr and never carry a token, a path or PDF text.
pub fn log(message: fmt::Arguments<'_>) {
    eprintln!("tpe-serve: {message}");
}

/// A job's public id, `<instance>-<n>`.
pub struct JobId<'a>(pub &'a str, pub usize);

impl fmt::Display for JobId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.0, self.1)
    }
}

impl Serialize for JobId<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The JSON of one job (docs/API.md, "Job").
struct JobView<'a> {
    instance: &'a str,
    row: &'a JobRow,
    meta: &'a Meta,
}

fn state(row: &JobRow) -> &'static str {
    match row.phase {
        Phase::Queued => "queued",
        Phase::Running if row.cancelling => "cancelling",
        Phase::Running => "running",
        Phase::Finished(_) => "finished",
        Phase::Failed(_) => "failed",
        Phase::Cancelled => "cancelled",
    }
}

/// A progress event in the shape `tpe extract --progress` prints, without
/// the path.
struct ProgressView(Progress);

impl Serialize for ProgressView {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Progress::Opened { pages, total } => {
                let mut s = serializer.serialize_struct("Progress", 3)?;
                s.serialize_field("event", "opened")?;
                s.serialize_field("pages", &pages)?;
                s.serialize_field("total", &total)?;
                s.end()
            }
            Progress::Page { page, done, total } => {
                let mut s = serializer.serialize_struct("Progress", 4)?;
                s.serialize_field("event", "page")?;
                s.serialize_field("page", &page)?;
                s.serialize_field("done", &done)?;
                s.serialize_field("total", &total)?;
                s.end()
            }
        }
    }
}

/// A job's outputs: index, file name, path and download URL.
struct OutputsView<'a>(&'a JobView<'a>, &'a [PathBuf]);

impl Serialize for OutputsView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Url<'a>(JobId<'a>, usize);
        impl fmt::Display for Url<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "/v1/jobs/{}/output/{}", self.0, self.1)
            }
        }
        #[derive(Serialize)]
        struct Output<'a> {
            index: usize,
            name: std::borrow::Cow<'a, str>,
            path: std::borrow::Cow<'a, str>,
            #[serde(serialize_with = "display")]
            url: Url<'a>,
        }
        fn display<S: Serializer>(url: &Url<'_>, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(url)
        }
        let mut seq = serializer.serialize_seq(Some(self.1.len()))?;
        for (index, path) in self.1.iter().enumerate() {
            seq.serialize_element(&Output {
                index,
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                path: path.to_string_lossy(),
                url: Url(JobId(self.0.instance, self.0.row.id), index),
            })?;
        }
        seq.end()
    }
}

impl Serialize for JobView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (summary, warnings, error, outputs): (
            Option<&str>,
            &[String],
            Option<&str>,
            &[PathBuf],
        ) = match &self.row.phase {
            Phase::Finished(outcome) => (
                Some(&outcome.summary),
                &outcome.warnings,
                None,
                &outcome.outputs,
            ),
            Phase::Failed(message) => (None, &[], Some(message), &[]),
            _ => (None, &[], None, &[]),
        };
        let mut s = serializer.serialize_struct("Job", 12)?;
        s.serialize_field("id", &JobId(self.instance, self.row.id))?;
        s.serialize_field("action", self.row.action.noun())?;
        s.serialize_field("path", &self.meta.sent)?;
        s.serialize_field("state", state(self.row))?;
        s.serialize_field("done", &self.row.done)?;
        s.serialize_field("total", &self.row.total)?;
        s.serialize_field("progress", &self.meta.last.map(ProgressView))?;
        s.serialize_field("summary", &summary)?;
        s.serialize_field("warnings", warnings)?;
        s.serialize_field("error", &error)?;
        s.serialize_field("outputs", &OutputsView(self, outputs))?;
        s.serialize_field("seq", &self.meta.seq)?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::redact;
    use std::path::Path;

    #[test]
    fn failures_name_only_what_the_client_sent() {
        let message = "writing next to /private/tmp/real/paper.pdf: disk full; \
                       creating /home/u/.local/share/PDFTextract: denied";
        assert_eq!(
            redact(
                message,
                Path::new("/private/tmp/real/paper.pdf"),
                "/tmp/link.pdf",
                Path::new("/home/u/.local/share/PDFTextract"),
            ),
            "writing next to /tmp/link.pdf: disk full; creating <state directory>: denied"
        );
    }
}
