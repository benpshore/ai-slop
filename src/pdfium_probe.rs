//! Character/geometry evidence directly from `PDFium`'s text-page API.
//!
//! The Rust controller snapshots a bounded local input and launches one worker.
//! Only the worker loads `PDFium`. No project `XObject` mapping or reading-order
//! reconstruction is involved. Linux rlimits and parent kill/wait are resource
//! and crash boundaries, not filesystem/network security sandboxing.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use pdfium_render::prelude::{FPDF_DOCUMENT, FPDF_PAGE, FPDF_TEXTPAGE, PdfiumLibraryBindings};
use serde::{Deserialize, Serialize};

use crate::backend::pdfium_backend::{PDFIUM_RENDER_VERSION, bind, library_file};
use crate::schema::sha256_hex;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub max_input_bytes: u64,
    pub max_pages: u16,
    pub max_chars: usize,
    pub max_output_bytes: u64,
    pub timeout_ms: u64,
    pub address_space_bytes: u64,
    pub cpu_seconds: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 64 * 1024 * 1024,
            max_pages: 50,
            max_chars: 250_000,
            max_output_bytes: 32 * 1024 * 1024,
            timeout_ms: 30_000,
            address_space_bytes: 512 * 1024 * 1024,
            cpu_seconds: 15,
        }
    }
}

impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_input_bytes > 0 && self.max_input_bytes <= 64 * 1024 * 1024,
            "input limit must be 1..=64 MiB"
        );
        ensure!(
            self.max_pages > 0 && self.max_chars > 0 && self.max_chars <= 250_000,
            "page/character limits must be positive; at most 250000 characters"
        );
        ensure!(
            (4096..=32 * 1024 * 1024).contains(&self.max_output_bytes),
            "output limit must be 4096 bytes..=32 MiB"
        );
        ensure!(
            (1..=300_000).contains(&self.timeout_ms),
            "timeout must be 1..=300000 ms"
        );
        ensure!(
            (64 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&self.address_space_bytes),
            "address-space limit must be 64 MiB..=2 GiB"
        );
        ensure!(
            (1..=60).contains(&self.cpu_seconds),
            "CPU limit must be 1..=60 seconds"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    Limited,
    Timeout,
    WorkerFailed,
    Unavailable,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppliedLimits {
    pub address_space_bytes: u64,
    pub cpu_seconds: u64,
    pub file_bytes: u64,
    pub core_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackendEvidence {
    pub name: String,
    pub binding_version: String,
    /// Actual library hash, not a claimed runtime version (`PDFium` has no API).
    pub library_sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Character {
    pub index: usize,
    pub unicode_value: u32,
    pub unicode_scalar: Option<char>,
    /// [left, bottom, right, top], `PDFium` page coordinates in points.
    pub tight_bounds: Option<[f32; 4]>,
    pub origin: Option<[f32; 2]>,
    pub font_size_points: Option<f32>,
    pub angle_radians: Option<f32>,
    pub generated: Option<bool>,
    pub hyphen: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub number: u32,
    pub width_points: f32,
    pub height_points: f32,
    pub rotation_degrees: u16,
    pub characters: Vec<Character>,
    pub unavailable_geometry: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub contract_version: u32,
    pub outcome: Outcome,
    pub detail: Option<String>,
    pub input_sha256: Option<String>,
    pub backend: Option<BackendEvidence>,
    pub limits: Limits,
    pub applied_limits: Option<AppliedLimits>,
    pub worker_pid: Option<u32>,
    pub worker_exit: Option<String>,
    pub elapsed_ms: u64,
    pub total_pages: Option<u32>,
    pub pages: Vec<Page>,
}

impl Report {
    fn new(limits: Limits) -> Self {
        Self {
            contract_version: 1,
            outcome: Outcome::Complete,
            detail: None,
            input_sha256: None,
            backend: None,
            limits,
            applied_limits: None,
            worker_pid: None,
            worker_exit: None,
            elapsed_ms: 0,
            total_pages: None,
            pages: vec![],
        }
    }
    fn stop(&mut self, outcome: Outcome, detail: impl Into<String>) {
        self.outcome = outcome;
        self.detail = Some(detail.into());
        // Incomplete artifacts are never represented as complete page evidence.
        self.pages.clear();
    }
}

/// A killed or crashed worker is always reaped, including early error paths.
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_worker(worker: &mut Worker, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = worker.0.try_wait()? {
            return Ok(Some(status));
        }
        if start.elapsed() >= timeout {
            worker.0.kill()?;
            worker.0.wait()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5).min(timeout.saturating_sub(start.elapsed())));
    }
}

/// Run one disposable worker using this executable's hidden `worker` command.
/// The wall deadline covers worker startup/loading/extraction/serialization.
pub fn run(input: &Path, limits: Limits) -> Result<Report> {
    limits.validate()?;
    let start = Instant::now();
    let mut report = Report::new(limits);
    if !cfg!(target_os = "linux") {
        report.stop(
            Outcome::Unavailable,
            "the containment prototype currently supports Linux only",
        );
        return Ok(report);
    }
    ensure!(
        input.metadata()?.is_file(),
        "input must be a completed regular file"
    );
    let temp = tempfile::tempdir()?;
    let snapshot = temp.path().join("input.pdf");
    let output = temp.path().join("result.json");
    let mut source = File::open(input)?.take(limits.max_input_bytes + 1);
    let bytes = io::copy(&mut source, &mut File::create(&snapshot)?)?;
    if bytes > limits.max_input_bytes {
        report.stop(Outcome::Limited, "input byte limit exceeded");
        return Ok(report);
    }
    report.input_sha256 = Some(sha256_hex(&fs::read(&snapshot)?));
    let child = Command::new(std::env::current_exe()?)
        .arg("worker")
        .arg(&snapshot)
        .arg(&output)
        .arg(serde_json::to_string(&limits)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut worker = Worker(child);
    report.worker_pid = Some(worker.0.id());
    let status = wait_worker(&mut worker, Duration::from_millis(limits.timeout_ms))?;
    report.elapsed_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    let Some(status) = status else {
        report.stop(
            Outcome::Timeout,
            "worker deadline exceeded; killed and reaped",
        );
        return Ok(report);
    };
    report.worker_exit = Some(status.to_string());
    if !status.success() {
        report.stop(
            Outcome::WorkerFailed,
            "worker exited unsuccessfully; native crash/resource exhaustion possible",
        );
        return Ok(report);
    }
    let mut bytes = Vec::new();
    if let Err(error) = File::open(&output)
        .and_then(|f| f.take(limits.max_output_bytes + 1).read_to_end(&mut bytes))
    {
        report.stop(
            Outcome::WorkerFailed,
            format!("worker output unavailable: {error}"),
        );
        return Ok(report);
    }
    if bytes.len() as u64 > limits.max_output_bytes {
        report.stop(Outcome::Limited, "worker output byte limit exceeded");
        return Ok(report);
    }
    let mut evidence: Report = match serde_json::from_slice(&bytes) {
        Ok(evidence) => evidence,
        Err(error) => {
            report.stop(
                Outcome::WorkerFailed,
                format!("invalid worker result: {error}"),
            );
            return Ok(report);
        }
    };
    ensure!(
        evidence.contract_version == 1 && evidence.input_sha256 == report.input_sha256,
        "worker identity mismatch"
    );
    evidence.worker_pid = report.worker_pid;
    evidence.worker_exit = report.worker_exit;
    evidence.elapsed_ms = report.elapsed_ms;
    Ok(evidence)
}

#[cfg(target_os = "linux")]
fn contain(limits: &Limits) -> Result<AppliedLimits> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    fn lower(resource: Resource, requested: u64) -> Result<u64> {
        let old = getrlimit(resource);
        let value = requested
            .min(old.current.unwrap_or(u64::MAX))
            .min(old.maximum.unwrap_or(u64::MAX));
        setrlimit(
            resource,
            Rlimit {
                current: Some(value),
                maximum: Some(value),
            },
        )?;
        Ok(value)
    }
    Ok(AppliedLimits {
        core_bytes: lower(Resource::Core, 0)?,
        address_space_bytes: lower(Resource::As, limits.address_space_bytes)?,
        cpu_seconds: lower(Resource::Cpu, limits.cpu_seconds)?,
        file_bytes: lower(Resource::Fsize, limits.max_output_bytes)?,
    })
}

#[cfg(not(target_os = "linux"))]
fn contain(_limits: &Limits) -> Result<AppliedLimits> {
    anyhow::bail!("Linux containment is unavailable")
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
    exceeded: bool,
}
impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "output limit exceeded",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Hidden child entry: apply limits before reading the PDF or binding `PDFium`.
pub fn worker(input: &Path, output: &Path, limits: Limits) -> Result<()> {
    limits.validate()?;
    let mut report = Report::new(limits);
    report.applied_limits = Some(contain(&limits).context("apply worker limits")?);
    let mut bytes = Vec::new();
    File::open(input)?
        .take(limits.max_input_bytes + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limits.max_input_bytes,
        "snapshot exceeds input limit"
    );
    report.input_sha256 = Some(sha256_hex(&bytes));
    if let Err(error) = extract(&bytes, &mut report) {
        report.stop(Outcome::WorkerFailed, format!("{error:#}"));
    }
    let mut writer = BoundedWriter {
        inner: File::create(output)?,
        // Reserve room for bounded controller PID/exit/timing metadata.
        remaining: limits.max_output_bytes - 1024,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, &report) {
        if !writer.exceeded {
            return Err(error.into());
        }
        report.stop(
            Outcome::Limited,
            "serialized evidence exceeds output byte limit",
        );
        // Discard the incomplete JSON and emit a bounded, explicit failure.
        writer = BoundedWriter {
            inner: File::create(output)?,
            remaining: limits.max_output_bytes - 1024,
            exceeded: false,
        };
        serde_json::to_writer(&mut writer, &report)?;
    }
    writer.flush()?;
    Ok(())
}

fn finite<const N: usize>(values: [f32; N]) -> Option<[f32; N]> {
    values.iter().all(|v| v.is_finite()).then_some(values)
}

// Scoped native handles borrow their owners; drop text before page before
// document before library. The native count API avoids pdfium-render 0.8's
// u16 page-count truncation on very large inputs.
struct NativeDocument<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    handle: FPDF_DOCUMENT,
    _bytes: &'a [u8],
}
impl Drop for NativeDocument<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            self.bindings.FPDF_CloseDocument(self.handle);
        }
    }
}
struct NativeTextPage<'a> {
    document: &'a NativeDocument<'a>,
    page: FPDF_PAGE,
    text: FPDF_TEXTPAGE,
}
impl Drop for NativeTextPage<'_> {
    fn drop(&mut self) {
        if !self.text.is_null() {
            self.document.bindings.FPDFText_ClosePage(self.text);
        }
        if !self.page.is_null() {
            self.document.bindings.FPDF_ClosePage(self.page);
        }
    }
}

fn native_bool(value: i32) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn extract(bytes: &[u8], report: &mut Report) -> Result<()> {
    let path = std::env::var("PDFIUM_DYNAMIC_LIB_PATH").unwrap_or_default();
    if !Path::new(&path).is_absolute() {
        report.stop(
            Outcome::Unavailable,
            "set PDFIUM_DYNAMIC_LIB_PATH to an absolute trusted library file/directory",
        );
        return Ok(());
    }
    let library = library_file(Path::new(&path));
    let pdfium = match bind(Some(&path)) {
        Ok(pdfium) => pdfium,
        Err(error) => {
            report.stop(Outcome::Unavailable, error.to_string());
            return Ok(());
        }
    };
    report.backend = Some(BackendEvidence {
        name: "pdfium-text-page".into(),
        binding_version: PDFIUM_RENDER_VERSION.into(),
        library_sha256: sha256_hex(&fs::read(library)?),
    });
    let bindings = pdfium.bindings();
    let document = NativeDocument {
        bindings,
        handle: bindings.FPDF_LoadMemDocument64(bytes, None),
        _bytes: bytes,
    };
    ensure!(
        !document.handle.is_null(),
        "PDFium open failed (native error {})",
        bindings.FPDF_GetLastError()
    );
    let count = bindings.FPDF_GetPageCount(document.handle);
    ensure!(count >= 0, "invalid native page count");
    report.total_pages = Some(count as u32);
    if count > i32::from(report.limits.max_pages) {
        report.stop(Outcome::Limited, "page limit exceeded");
        return Ok(());
    }
    let mut total_chars = 0;
    for index in 0..count {
        let mut page = NativeTextPage {
            document: &document,
            page: bindings.FPDF_LoadPage(document.handle, index),
            text: std::ptr::null_mut(),
        };
        ensure!(!page.page.is_null(), "PDFium page {} failed", index + 1);
        page.text = bindings.FPDFText_LoadPage(page.page);
        ensure!(
            !page.text.is_null(),
            "PDFium text page {} failed",
            index + 1
        );
        let chars = bindings.FPDFText_CountChars(page.text);
        ensure!(chars >= 0, "invalid native character count");
        if chars as usize > report.limits.max_chars - total_chars {
            report.stop(Outcome::Limited, "character limit exceeded");
            return Ok(());
        }
        total_chars += chars as usize;
        let rotation = bindings.FPDFPage_GetRotation(page.page);
        ensure!((0..=3).contains(&rotation), "invalid native rotation");
        let mut evidence = Page {
            number: (index + 1) as u32,
            width_points: bindings.FPDF_GetPageWidthF(page.page),
            height_points: bindings.FPDF_GetPageHeightF(page.page),
            rotation_degrees: rotation as u16 * 90,
            characters: Vec::with_capacity(chars as usize),
            unavailable_geometry: 0,
        };
        ensure!(
            evidence.width_points.is_finite() && evidence.height_points.is_finite(),
            "nonfinite page dimensions"
        );
        for character in 0..chars {
            let (mut left, mut right, mut bottom, mut top) = (0.0, 0.0, 0.0, 0.0);
            let tight_bounds = (bindings.FPDFText_GetCharBox(
                page.text,
                character,
                &raw mut left,
                &raw mut right,
                &raw mut bottom,
                &raw mut top,
            ) != 0)
                .then(|| finite([left as f32, bottom as f32, right as f32, top as f32]))
                .flatten();
            let (mut x, mut y) = (0.0, 0.0);
            let origin = (bindings
                .FPDFText_GetCharOrigin(page.text, character, &raw mut x, &raw mut y)
                != 0)
                .then(|| finite([x as f32, y as f32]))
                .flatten();
            if tight_bounds.is_none() || origin.is_none() {
                evidence.unavailable_geometry += 1;
            }
            let unicode_value = bindings.FPDFText_GetUnicode(page.text, character);
            evidence.characters.push(Character {
                index: character as usize,
                unicode_value,
                unicode_scalar: (unicode_value != 0)
                    .then(|| char::from_u32(unicode_value))
                    .flatten(),
                tight_bounds,
                origin,
                font_size_points: finite([
                    bindings.FPDFText_GetFontSize(page.text, character) as f32
                ])
                .map(|v| v[0]),
                angle_radians: finite([bindings.FPDFText_GetCharAngle(page.text, character)])
                    .map(|v| v[0]),
                generated: native_bool(bindings.FPDFText_IsGenerated(page.text, character)),
                hyphen: native_bool(bindings.FPDFText_IsHyphen(page.text, character)),
            });
        }
        report.pages.push(evidence);
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    #[ignore = "child process used by worker_kernel_limits_leave_controller_unchanged"]
    fn containment_child() {
        if std::env::var_os("TPE_PROBE_CONTAINMENT_CHILD").is_none() {
            return;
        }
        let limits = Limits {
            address_space_bytes: 128 * 1024 * 1024,
            cpu_seconds: 1,
            ..Limits::default()
        };
        contain(&limits).unwrap();
        let mut bytes = Vec::<u8>::new();
        assert!(
            bytes.try_reserve_exact(256 * 1024 * 1024).is_err(),
            "address-space limit must reject allocation"
        );
        loop {
            std::hint::black_box(1_u64.wrapping_add(1));
        }
    }

    #[test]
    fn worker_kernel_limits_leave_controller_unchanged() {
        use rustix::process::{Resource, getrlimit};
        let before = [Resource::As, Resource::Cpu, Resource::Fsize, Resource::Core].map(getrlimit);
        let mut child = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pdfium_probe::tests::containment_child",
                    "--ignored",
                ])
                .env("TPE_PROBE_CONTAINMENT_CHILD", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let status = wait_worker(&mut child, Duration::from_secs(10))
            .unwrap()
            .expect("CPU limit should terminate before deadline");
        assert!(matches!(status.signal(), Some(9 | 24)), "{status}");
        assert_eq!(
            before,
            [Resource::As, Resource::Cpu, Resource::Fsize, Resource::Core].map(getrlimit)
        );
    }

    #[test]
    fn deadline_kills_and_reaps_and_exit_failure_is_observable() {
        let child = Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut worker = Worker(child);
        let start = Instant::now();
        assert!(
            wait_worker(&mut worker, Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "worker must be reaped"
        );
        let mut next = Worker(Command::new("sh").args(["-c", "exit 17"]).spawn().unwrap());
        assert_eq!(
            wait_worker(&mut next, Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .code(),
            Some(17)
        );
        let mut crashed = Worker(
            Command::new("sh")
                .args(["-c", "ulimit -c 0; kill -SEGV $$"])
                .spawn()
                .unwrap(),
        );
        assert_eq!(
            wait_worker(&mut crashed, Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .signal(),
            Some(11)
        );
    }

    #[test]
    fn writer_never_exceeds_its_bound() {
        let mut writer = BoundedWriter {
            inner: Vec::new(),
            remaining: 4,
            exceeded: false,
        };
        writer.write_all(b"1234").unwrap();
        assert!(writer.write_all(b"5").is_err());
        assert_eq!(writer.inner, b"1234");
        assert!(writer.exceeded);
    }
}
