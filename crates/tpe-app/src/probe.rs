//! A responsiveness probe for the real app. Off unless `PDFTEXTRACT_PROBE` is
//! set; no dependencies beyond `serde_json`, which the crate already has.
//!
//! The GPUI test platform behind `interaction_timings` (docs/APP.md) has no
//! GPU, no display and no event loop, so it cannot show a dropped frame or a
//! stalled main thread. This module records what the *running* app does, as
//! distributions, never a median alone:
//!
//! - series of durations in milliseconds (main-thread event-loop latency,
//!   frame-to-frame interval, input-to-frame latency, the time the main
//!   thread spends in named pieces of work),
//! - the phase the app was in when each sample was taken (idle, a batch of N
//!   files arriving, jobs running), so a stall can be tied to what caused it,
//! - marks (job started, N files arrived, text size changed) with timestamps.
//!
//! The report is JSON ([`Probe::report`]) with a plain-text rendering
//! ([`render_text`]) of the same data. This module is pure Rust and tested on
//! every platform; the GPUI side that feeds it is `gui/instrument.rs`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The environment variable that turns the probe on: the path of the JSON
/// report (`1` or empty means a file in the temporary directory).
pub const ENV: &str = "PDFTEXTRACT_PROBE";

/// Free text the launcher (`probe.sh`) attaches to the report: the machine,
/// power source, display.
pub const NOTE_ENV: &str = "PDFTEXTRACT_PROBE_NOTE";

/// Stall thresholds every series is counted against, in milliseconds: half a
/// 60 Hz frame, one 60 Hz frame, and a delay a person notices as a hang.
pub const THRESHOLDS_MS: [f64; 3] = [8.0, 16.7, 50.0];

/// Samples kept per series (12 bytes each); further ones are counted only.
const MAX_SAMPLES: usize = 1_500_000;
/// Marks kept.
const MAX_MARKS: usize = 20_000;
/// Stalls listed per series in the report (the worst ones, in time order).
const MAX_STALLS: usize = 300;

#[derive(Clone, Copy)]
struct Sample {
    /// When the measured thing ended, in milliseconds since the probe started.
    end_ms: u32,
    micros: u32,
    phase: u16,
}

#[derive(Default)]
struct Series {
    samples: Vec<Sample>,
    dropped: u64,
    /// A frame-to-frame interval: a stall is a missed refresh (1.5x the
    /// median), not "longer than 8 ms", which every 60 Hz frame is.
    interval: bool,
    /// Main-thread work: overlapping stalls are attributed to it.
    span: bool,
}

struct State {
    series: BTreeMap<&'static str, Series>,
    counters: BTreeMap<&'static str, u64>,
    marks: Vec<(u32, String)>,
    phases: Vec<(u32, String)>,
}

/// The recorder. One per process ([`init`], [`get`]).
pub struct Probe {
    started: Instant,
    path: PathBuf,
    state: Mutex<State>,
}

static PROBE: OnceLock<Probe> = OnceLock::new();

/// Enable the probe if [`ENV`] is set. Returns whether it is on. Safe to
/// call more than once.
pub fn init() -> bool {
    let Some(value) = std::env::var_os(ENV) else {
        return false;
    };
    let path = if value.is_empty() || value == "1" {
        std::env::temp_dir().join("pdftextract-probe.json")
    } else {
        PathBuf::from(value)
    };
    init_at(path)
}

/// Enable the probe, writing its report to `path` ([`init`] without the
/// environment). The first call in a process wins.
pub fn init_at(path: PathBuf) -> bool {
    PROBE.get_or_init(|| Probe::new(path));
    true
}

/// The probe, when enabled. One atomic load when it is not.
pub fn get() -> Option<&'static Probe> {
    PROBE.get()
}

/// Time a piece of main-thread work: `let _t = probe::span("name");`.
/// `None` (nothing recorded, nothing timed) when the probe is off.
pub fn span(name: &'static str) -> Option<SpanGuard> {
    get().map(|probe| SpanGuard {
        probe,
        name,
        start: Instant::now(),
    })
}

/// Records the time since [`span`] when dropped.
pub struct SpanGuard {
    probe: &'static Probe,
    name: &'static str,
    start: Instant,
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        self.probe
            .record_with(self.name, self.start.elapsed(), Kind::Span);
    }
}

/// What a series measures, which decides how its stalls are found.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A delay or duration.
    Delay,
    /// A frame-to-frame interval.
    Interval,
    /// Main-thread work; overlapping stalls are attributed to it.
    Span,
}

fn micros(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

impl Probe {
    fn new(path: PathBuf) -> Self {
        Self {
            started: Instant::now(),
            path,
            state: Mutex::new(State {
                series: BTreeMap::new(),
                counters: BTreeMap::new(),
                marks: Vec::new(),
                phases: vec![(0, "start".to_string())],
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Milliseconds since the probe started.
    pub fn now_ms(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX)
    }

    /// Where the JSON report is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one duration into `series`, ending now.
    pub fn record(&self, series: &'static str, duration: Duration) {
        self.record_with(series, duration, Kind::Delay);
    }

    /// Record one duration into `series`, ending now, of the given kind.
    pub fn record_with(&self, series: &'static str, duration: Duration, kind: Kind) {
        let end_ms = self.now_ms();
        let mut state = self.lock();
        let phase = u16::try_from(state.phases.len() - 1).unwrap_or(u16::MAX);
        let entry = state.series.entry(series).or_default();
        entry.interval |= kind == Kind::Interval;
        entry.span |= kind == Kind::Span;
        if entry.samples.len() < MAX_SAMPLES {
            entry.samples.push(Sample {
                end_ms,
                micros: micros(duration),
                phase,
            });
        } else {
            entry.dropped += 1;
        }
    }

    /// Add `n` to a counter.
    pub fn count(&self, name: &'static str, n: u64) {
        *self.lock().counters.entry(name).or_default() += n;
    }

    /// A timestamped event, listed in the report and used to explain stalls.
    pub fn mark(&self, label: impl Into<String>) {
        let at = self.now_ms();
        let mut state = self.lock();
        if state.marks.len() < MAX_MARKS {
            state.marks.push((at, label.into()));
        }
    }

    /// Start a new phase: later samples are grouped under it as well as in
    /// the whole-run summary.
    pub fn phase(&self, label: impl Into<String>) {
        let at = self.now_ms();
        let label = label.into();
        let mut state = self.lock();
        if state.phases.last().is_some_and(|(_, last)| *last == label) {
            return;
        }
        if state.marks.len() < MAX_MARKS {
            state.marks.push((at, format!("phase: {label}")));
        }
        if state.phases.len() < usize::from(u16::MAX) {
            state.phases.push((at, label));
        }
    }

    /// The report as JSON.
    pub fn report(&self) -> Value {
        let elapsed_ms = self.now_ms();
        // Copy under the lock, compute outside it: the main thread records
        // into this mutex and must not wait for a sort.
        let (series, counters, marks, phases) = {
            let state = self.lock();
            let series: Vec<(&'static str, Series)> = state
                .series
                .iter()
                .map(|(name, s)| {
                    (
                        *name,
                        Series {
                            samples: s.samples.clone(),
                            dropped: s.dropped,
                            interval: s.interval,
                            span: s.span,
                        },
                    )
                })
                .collect();
            (
                series,
                state.counters.clone(),
                state.marks.clone(),
                state.phases.clone(),
            )
        };
        let mut spans: Vec<(u32, u32, &'static str)> = series
            .iter()
            .filter(|(_, s)| s.span)
            .flat_map(|(name, s)| s.samples.iter().map(|x| (x.end_ms, x.micros, *name)))
            .collect();
        spans.sort_unstable();
        let mut out = serde_json::Map::new();
        for (name, s) in &series {
            out.insert((*name).to_string(), series_json(s, &phases, &marks, &spans));
        }
        json!({
            "meta": Self::meta(elapsed_ms),
            "series": Value::Object(out),
            "counters": counters,
            "marks": marks.iter().map(|(t, l)| json!({"t_ms": t, "label": l})).collect::<Vec<_>>(),
            "phases": phases.iter().map(|(t, l)| json!({"t_ms": t, "label": l})).collect::<Vec<_>>(),
        })
    }

    fn meta(elapsed_ms: u32) -> Value {
        json!({
            "probe": 1,
            "app_version": option_env!("PDFTEXTRACT_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "available_parallelism": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
            "duration_ms": elapsed_ms,
            "note": std::env::var(NOTE_ENV).unwrap_or_default(),
            "thresholds_ms": THRESHOLDS_MS,
            "how_to_read": "Times are milliseconds. Every series shows n, min, median, p95, p99 (only when n >= 100), max and max/median, and how many samples exceeded 8, 16.7 and 50 ms. Outliers are the finding, not the median.",
        })
    }

    /// Write the JSON report and its text rendering (`.txt` beside it),
    /// each atomically. Errors are returned, never panicked on.
    ///
    /// # Errors
    /// Any I/O error writing either file.
    pub fn write(&self) -> std::io::Result<()> {
        let report = self.report();
        write_atomically(
            &self.path,
            serde_json::to_string_pretty(&report)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        write_atomically(
            &self.path.with_extension("txt"),
            render_text(&report).as_bytes(),
        )
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// Distribution of a set of durations, in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub median: f64,
    pub p95: f64,
    /// Only with at least 100 samples: below that it is just the maximum.
    pub p99: Option<f64>,
    pub max: f64,
    pub max_over_median: f64,
    /// Samples above [`THRESHOLDS_MS`], in order.
    pub over: [usize; 3],
}

/// The nearest-rank percentile of sorted values.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Summarise `values_ms`; `None` when empty.
pub fn summarize(values_ms: &[f64]) -> Option<Summary> {
    if values_ms.is_empty() {
        return None;
    }
    let mut sorted = values_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        f64::midpoint(sorted[n / 2 - 1], sorted[n / 2])
    };
    let max = sorted[n - 1];
    let over = THRESHOLDS_MS.map(|limit| sorted.iter().filter(|v| **v > limit).count());
    Some(Summary {
        n,
        min: sorted[0],
        median,
        p95: percentile(&sorted, 95.0),
        p99: (n >= 100).then(|| percentile(&sorted, 99.0)),
        max,
        max_over_median: max / median.max(f64::EPSILON),
        over,
    })
}

fn r3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

impl Summary {
    fn to_json(&self) -> Value {
        json!({
            "n": self.n,
            "min": r3(self.min),
            "median": r3(self.median),
            "p95": r3(self.p95),
            "p99": self.p99.map(r3),
            "max": r3(self.max),
            "max_over_median": r3(self.max_over_median),
            "over_8ms": self.over[0],
            "over_16_7ms": self.over[1],
            "over_50ms": self.over[2],
        })
    }
}

fn ms(sample: Sample) -> f64 {
    f64::from(sample.micros) / 1000.0
}

fn series_json(
    series: &Series,
    phases: &[(u32, String)],
    marks: &[(u32, String)],
    spans: &[(u32, u32, &'static str)],
) -> Value {
    let values: Vec<f64> = series.samples.iter().copied().map(ms).collect();
    let Some(overall) = summarize(&values) else {
        return json!({"n": 0});
    };
    let mut out = overall.to_json();
    let object = out.as_object_mut().expect("a summary is an object");
    object.insert("dropped_after_cap".into(), json!(series.dropped));

    // A missed refresh: an interval of at least 1.5 medians, counted in
    // refreshes lost (a 50 ms gap at a 16.7 ms period is 2 lost).
    let stall_ms = if series.interval {
        let period = overall.median;
        let mut lost = 0u64;
        let mut long = 0usize;
        for value in &values {
            if *value >= 1.5 * period {
                long += 1;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    lost += ((value / period).round() as u64).saturating_sub(1);
                }
            }
        }
        object.insert("refresh_period_ms_estimate".into(), json!(r3(period)));
        object.insert("intervals_at_least_1_5_periods".into(), json!(long));
        object.insert("refreshes_lost_estimate".into(), json!(lost));
        (1.5 * period).max(THRESHOLDS_MS[0])
    } else {
        THRESHOLDS_MS[0]
    };
    object.insert("stall_listing_threshold_ms".into(), json!(r3(stall_ms)));

    // Per phase.
    let mut by_phase: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for sample in &series.samples {
        let label = phases
            .get(usize::from(sample.phase))
            .map_or("?", |(_, label)| label.as_str());
        by_phase.entry(label).or_default().push(ms(*sample));
    }
    let by_phase: serde_json::Map<String, Value> = by_phase
        .into_iter()
        .filter_map(|(label, values)| Some((label.to_string(), summarize(&values)?.to_json())))
        .collect();
    object.insert("by_phase".into(), Value::Object(by_phase));

    // The worst stalls with what was going on: the last mark before, the
    // phase, and any main-thread work that overlapped the stall.
    let mut stalls: Vec<Sample> = series
        .samples
        .iter()
        .copied()
        .filter(|s| ms(*s) > stall_ms)
        .collect();
    let stall_count = stalls.len();
    stalls.sort_by_key(|s| std::cmp::Reverse(s.micros));
    stalls.truncate(MAX_STALLS);
    stalls.sort_by_key(|s| s.end_ms);
    let listed: Vec<Value> = stalls
        .iter()
        .map(|s| {
            let end = s.end_ms;
            let start = end.saturating_sub(s.micros / 1000);
            let mark = marks
                .iter()
                .rev()
                .find(|(t, _)| *t <= end)
                .map(|(t, l)| format!("{l} (at {t} ms)"));
            let phase = phases
                .get(usize::from(s.phase))
                .map(|(_, label)| label.clone());
            json!({
                "end_ms": end,
                "ms": r3(ms(*s)),
                "phase": phase,
                "last_mark": mark,
                "overlapping_main_thread_work": overlapping(spans, start, end, series.span),
            })
        })
        .collect();
    object.insert("stall_count".into(), json!(stall_count));
    object.insert("stalls".into(), Value::Array(listed));
    out
}

/// The named main-thread spans of at least 1 ms that overlap `[start, end]`
/// (milliseconds), largest first. Empty for a series that is itself work.
fn overlapping(
    spans: &[(u32, u32, &'static str)],
    start: u32,
    end: u32,
    itself: bool,
) -> Vec<String> {
    if itself {
        return Vec::new();
    }
    // Spans are sorted by end time: skip those that ended before the stall.
    let first = spans.partition_point(|(span_end, _, _)| *span_end < start);
    let mut found: Vec<(u32, &'static str)> = spans[first..]
        .iter()
        .take_while(|(span_end, micros, _)| span_end.saturating_sub(micros / 1000) <= end)
        .filter(|(_, micros, _)| *micros >= 1000)
        .map(|(_, micros, name)| (*micros, *name))
        .collect();
    found.sort_by_key(|(micros, _)| std::cmp::Reverse(*micros));
    found
        .into_iter()
        .take(4)
        .map(|(micros, name)| format!("{name} {:.1} ms", f64::from(micros) / 1000.0))
        .collect()
}

/// Add a series of durations that came from outside the probe (GPUI's own
/// frame timings, parsed by [`parse_frame_durations`]) to a report.
pub fn merge_series(report: &mut Value, name: &str, values_ms: &[f64]) {
    let Some(summary) = summarize(values_ms) else {
        return;
    };
    if let Some(series) = report.get_mut("series").and_then(Value::as_object_mut) {
        series.insert(name.to_string(), summary.to_json());
    }
}

/// The name [`merge_series`] gives GPUI's own frame timings.
pub const GPUI_FRAME_SERIES: &str =
    "GPUI draw+present, CPU time per drawn frame (ZED_MEASUREMENTS)";

/// Milliseconds from the `frame duration: 1.234ms` lines GPUI 0.2.2 prints
/// to stderr when `ZED_MEASUREMENTS=1` (`gpui_util::measure`, wrapped around
/// `Window::draw` + `present` in `window.rs`).
pub fn parse_frame_durations(log: &str) -> Vec<f64> {
    log.lines()
        .filter_map(|line| line.strip_prefix("frame duration: "))
        .filter_map(|text| {
            let text = text.trim();
            // `Duration`'s Debug output: 1.5s, 12.3ms, 45.6µs, 789ns.
            let (number, scale) = if let Some(n) = text.strip_suffix("ms") {
                (n, 1.0)
            } else if let Some(n) = text.strip_suffix("µs") {
                (n, 1e-3)
            } else if let Some(n) = text.strip_suffix("ns") {
                (n, 1e-6)
            } else {
                (text.strip_suffix('s')?, 1e3)
            };
            number.parse::<f64>().ok().map(|v| v * scale)
        })
        .collect()
}

/// A plain, uncompressed PDF of `pages` pages of ordinary body text, for
/// driving jobs longer than the two-page fixture (`tpe-app --probe-make-pdf`).
/// Every page is a different 42 lines, so nothing is cached between them.
pub fn synthetic_pdf(pages: usize) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
    let mut offsets: Vec<usize> = Vec::new();
    let mut object = |out: &mut Vec<u8>, body: &str| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", offsets.len()).as_bytes());
    };
    object(&mut out, "<< /Type /Catalog /Pages 2 0 R >>");
    let mut kids = String::new();
    for page in 0..pages {
        let _ = write!(kids, "{} 0 R ", 4 + 2 * page);
    }
    object(
        &mut out,
        &format!("<< /Type /Pages /Kids [ {kids}] /Count {pages} >>"),
    );
    object(
        &mut out,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    );
    for page in 0..pages {
        let content_id = 5 + 2 * page;
        object(
            &mut out,
            &format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
            ),
        );
        let mut stream = String::from("BT /F1 10 Tf 12 TL 54 740 Td\n");
        for line in 0..42 {
            let _ = writeln!(
                stream,
                "(Page {page} line {line}: the committee reviewed the measured response of \
                 sample {} and recorded the result in table {}.) Tj T*",
                page * 42 + line,
                (page + line) % 9 + 1
            );
        }
        stream.push_str("ET");
        object(
            &mut out,
            &format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
        );
    }
    let xref = out.len();
    let count = offsets.len() + 1;
    let mut table = format!("xref\n0 {count}\n0000000000 65535 f \n");
    for offset in &offsets {
        let _ = writeln!(table, "{offset:010} 00000 n ");
    }
    let _ = write!(
        table,
        "trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    );
    out.extend_from_slice(table.as_bytes());
    out
}

/// `tpe-app --probe-merge STDERR_LOG REPORT_JSON`: add GPUI's own frame
/// timings from the app's stderr to a report and rewrite its text form.
///
/// # Errors
/// Reading either file, parsing the report, or writing it back.
pub fn merge_gpui_log(log: &Path, report: &Path) -> std::io::Result<usize> {
    let durations = parse_frame_durations(&std::fs::read_to_string(log).unwrap_or_default());
    let mut json: Value = serde_json::from_str(&std::fs::read_to_string(report)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    merge_series(&mut json, GPUI_FRAME_SERIES, &durations);
    write_atomically(
        report,
        serde_json::to_string_pretty(&json)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    write_atomically(&report.with_extension("txt"), render_text(&json).as_bytes())?;
    Ok(durations.len())
}

fn cell(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_f64)
        .map_or_else(|| "-".to_string(), |f| format!("{f:.2}"))
}

fn table_row(label: &str, summary: &Value) -> String {
    format!(
        "| {label} | {} | {} | {} | {} | {} | {} | {}x | {} | {} | {} |\n",
        summary.get("n").and_then(Value::as_u64).unwrap_or(0),
        cell(summary, "min"),
        cell(summary, "median"),
        cell(summary, "p95"),
        cell(summary, "p99"),
        cell(summary, "max"),
        cell(summary, "max_over_median"),
        summary.get("over_8ms").and_then(Value::as_u64).unwrap_or(0),
        summary
            .get("over_16_7ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        summary
            .get("over_50ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

const TABLE_HEAD: &str =
    "n | min | median | p95 | p99 | max | max/median | >8 ms | >16.7 ms | >50 ms |";
const RULE: &str = "---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |";

/// The worst stalls of one series, in time order, with what overlapped them.
fn render_stalls(out: &mut String, name: &str, summary: &Value) {
    let Some(stalls) = summary["stalls"].as_array().filter(|s| !s.is_empty()) else {
        return;
    };
    let _ = writeln!(
        out,
        "Stalls: {name} ({} above {} ms; the worst {} shown in time order)",
        summary["stall_count"],
        summary["stall_listing_threshold_ms"],
        stalls.len().min(25),
    );
    let mut worst: Vec<&Value> = stalls.iter().collect();
    worst.sort_by(|a, b| {
        b["ms"]
            .as_f64()
            .partial_cmp(&a["ms"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    worst.truncate(25);
    worst.sort_by_key(|s| s["end_ms"].as_u64());
    for stall in worst {
        let work = stall["overlapping_main_thread_work"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  t={:>8} ms  {:>8.1} ms  phase: {}  after: {}  main-thread work then: {}",
            stall["end_ms"],
            stall["ms"].as_f64().unwrap_or(0.0),
            stall["phase"].as_str().unwrap_or("?"),
            stall["last_mark"].as_str().unwrap_or("-"),
            work,
        );
    }
    out.push('\n');
}

/// A plain-text rendering of [`Probe::report`] (or of a report a file holds).
pub fn render_text(report: &Value) -> String {
    let mut out = String::new();
    let meta = &report["meta"];
    let _ = writeln!(out, "PDFTextract responsiveness probe");
    let _ = writeln!(
        out,
        "app {} ({} build), {} {}, {} logical CPUs, ran {:.1} s",
        meta["app_version"].as_str().unwrap_or("?"),
        meta["build"].as_str().unwrap_or("?"),
        meta["os"].as_str().unwrap_or("?"),
        meta["arch"].as_str().unwrap_or("?"),
        meta["available_parallelism"],
        meta["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0,
    );
    if let Some(note) = meta["note"].as_str().filter(|n| !n.is_empty()) {
        let _ = writeln!(out, "machine: {note}");
    }
    let _ = writeln!(
        out,
        "All times in milliseconds. Outliers are the finding: read max and the >N ms counts, not the median.\n"
    );
    let Some(series) = report["series"].as_object() else {
        return out;
    };
    let _ = writeln!(out, "| series | {TABLE_HEAD}\n| --- | {RULE}");
    for (name, summary) in series {
        if summary["n"].as_u64().unwrap_or(0) > 0 {
            out.push_str(&table_row(name, summary));
        }
    }
    out.push('\n');
    for (name, summary) in series {
        let Some(phases) = summary["by_phase"].as_object().filter(|p| p.len() > 1) else {
            continue;
        };
        let _ = writeln!(out, "By phase: {name}");
        let _ = writeln!(out, "| phase | {TABLE_HEAD}\n| --- | {RULE}");
        for (label, phase) in phases {
            out.push_str(&table_row(label, phase));
        }
        out.push('\n');
    }
    if let Some(counters) = report["counters"].as_object().filter(|c| !c.is_empty()) {
        let _ = writeln!(out, "Counters");
        for (name, value) in counters {
            let _ = writeln!(out, "  {name}: {value}");
        }
        out.push('\n');
    }
    for (name, summary) in series {
        render_stalls(&mut out, name, summary);
    }
    if let Some(marks) = report["marks"].as_array().filter(|m| !m.is_empty()) {
        let _ = writeln!(out, "Marks (first 60)");
        for mark in marks.iter().take(60) {
            let _ = writeln!(
                out,
                "  t={:>8} ms  {}",
                mark["t_ms"],
                mark["label"].as_str().unwrap_or("")
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reports_range_and_tail_not_just_the_middle() {
        let mut values = vec![1.0; 99];
        values.push(80.0);
        let s = summarize(&values).unwrap();
        assert_eq!(s.n, 100);
        assert!((s.median - 1.0).abs() < 1e-9);
        assert!((s.max - 80.0).abs() < 1e-9);
        assert!((s.max_over_median - 80.0).abs() < 1e-9);
        // Nearest rank: the 99th of 100 sorted values is still 1.0.
        assert_eq!(s.p99, Some(1.0));
        assert_eq!(s.over, [1, 1, 1]);
    }

    #[test]
    fn p99_needs_a_hundred_samples() {
        assert_eq!(summarize(&[1.0; 99]).unwrap().p99, None);
        assert!(summarize(&[1.0; 100]).unwrap().p99.is_some());
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn the_median_of_an_even_count_averages_the_middle_pair() {
        assert!((summarize(&[1.0, 2.0, 3.0, 10.0]).unwrap().median - 2.5).abs() < 1e-9);
    }

    #[test]
    fn thresholds_count_strictly_above() {
        let s = summarize(&[8.0, 8.1, 16.7, 16.8, 50.0, 50.1]).unwrap();
        assert_eq!(s.over, [5, 3, 1]);
    }

    fn probe() -> Probe {
        Probe::new(std::env::temp_dir().join("tpe-probe-test.json"))
    }

    #[test]
    fn stalls_carry_the_phase_the_mark_and_the_overlapping_work() {
        let probe = probe();
        probe.phase("batch of 500 files");
        probe.mark("500 files arrived");
        // A 40 ms piece of main-thread work that ends now, and a delay of
        // 40 ms measured over the same time.
        probe.record_with("main: enqueue", Duration::from_millis(40), Kind::Span);
        probe.record("main-thread event-loop latency", Duration::from_millis(40));
        probe.record("main-thread event-loop latency", Duration::from_micros(200));
        let report = probe.report();
        let latency = &report["series"]["main-thread event-loop latency"];
        assert_eq!(latency["n"], 2);
        assert_eq!(latency["stall_count"], 1);
        let stall = &latency["stalls"][0];
        assert_eq!(stall["phase"], "batch of 500 files");
        assert!(
            stall["last_mark"]
                .as_str()
                .unwrap()
                .contains("500 files arrived")
        );
        let work = stall["overlapping_main_thread_work"][0].as_str().unwrap();
        assert!(work.starts_with("main: enqueue 40"), "{work}");
        // Per-phase summary exists next to the overall one.
        assert_eq!(latency["by_phase"]["batch of 500 files"]["n"], 2);
        // Work is never blamed for itself.
        let own = &report["series"]["main: enqueue"];
        assert_eq!(own["stalls"][0]["overlapping_main_thread_work"], json!([]));
    }

    #[test]
    fn an_interval_series_counts_missed_refreshes_not_every_frame() {
        let probe = probe();
        for _ in 0..10 {
            probe.record_with(
                "frame interval",
                Duration::from_micros(16_700),
                Kind::Interval,
            );
        }
        probe.record_with("frame interval", Duration::from_millis(50), Kind::Interval);
        let report = probe.report();
        let frames = &report["series"]["frame interval"];
        assert_eq!(frames["stall_count"], 1, "only the 50 ms gap is a stall");
        assert_eq!(frames["intervals_at_least_1_5_periods"], 1);
        // 50 / 16.7 rounds to 3 refreshes, so 2 were lost.
        assert_eq!(frames["refreshes_lost_estimate"], 2);
    }

    #[test]
    fn a_repeated_phase_is_not_a_new_phase() {
        let probe = probe();
        probe.phase("idle");
        probe.phase("idle");
        probe.phase("busy");
        assert_eq!(probe.report()["phases"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn the_report_is_written_as_json_and_text() {
        let dir = std::env::temp_dir().join(format!("tpe-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let probe = Probe::new(dir.join("r.json"));
        probe.record("main-thread event-loop latency", Duration::from_millis(3));
        probe.write().unwrap();
        let json: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("r.json")).unwrap()).unwrap();
        assert_eq!(json["series"]["main-thread event-loop latency"]["n"], 1);
        let text = std::fs::read_to_string(dir.join("r.txt")).unwrap();
        assert!(text.contains("main-thread event-loop latency"), "{text}");
        assert!(text.contains("max/median"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn gpui_frame_durations_parse_in_every_unit() {
        let log = "noise\nframe duration: 1.5ms\nframe duration: 250µs\nframe duration: 2s\nframe duration: 500ns\nframe duration: bad\n";
        let values = parse_frame_durations(log);
        assert_eq!(values.len(), 4);
        assert!((values[0] - 1.5).abs() < 1e-9);
        assert!((values[1] - 0.25).abs() < 1e-9);
        assert!((values[2] - 2000.0).abs() < 1e-9);
        assert!((values[3] - 0.0005).abs() < 1e-9);
    }

    #[test]
    fn merged_series_appear_in_the_text_report() {
        let mut report = probe().report();
        merge_series(&mut report, GPUI_FRAME_SERIES, &[1.0, 2.0, 30.0]);
        let text = render_text(&report);
        assert!(text.contains("GPUI draw+present"), "{text}");
    }

    #[test]
    fn the_synthetic_pdf_has_the_pages_asked_for_and_extracts() {
        use crate::jobs::{self, Action, CancelToken};
        let dir = std::env::temp_dir().join(format!("tpe-probe-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("long.pdf");
        std::fs::write(&pdf, synthetic_pdf(30)).unwrap();
        let mut events = 0;
        let outcome = jobs::run(
            Action::Text,
            &pdf,
            &dir.join("ledger.sqlite"),
            &mut |_| events += 1,
            &CancelToken::new(),
        )
        .unwrap();
        assert!(outcome.summary.contains("30 pages"), "{}", outcome.summary);
        assert_eq!(events, 31, "one Opened and one Page event per page");
        let text = std::fs::read_to_string(&outcome.outputs[0]).unwrap();
        assert!(
            text.contains("Page 29 line 41"),
            "the last page's text is there"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
