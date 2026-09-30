//! The GPUI side of the responsiveness probe (`tpe_app::probe`). Everything
//! here is inert unless `PDFTEXTRACT_PROBE` is set.
//!
//! What it measures, on the running app (not GPUI's test platform):
//!
//! - **Event-loop latency** ([`EVENT_LOOP`]): a watchdog thread wakes every
//!   few milliseconds and posts a message to a foreground task; the series is
//!   how long the message waited before the main thread ran it. Every post is
//!   timed, including the ones that queued behind a stall, so a 100 ms block
//!   shows as ~25 late samples, not one. The watchdog's own oversleep
//!   ([`WATCHDOG`]) is the control: if it is late too, the whole machine was
//!   starved, not just the main thread.
//! - **Frame pacing** ([`FRAME`]): `Window::on_next_frame` callbacks run at
//!   the start of every display-link tick the main thread handles (GPUI
//!   0.2.2 `window.rs`, the `on_request_frame` closure), drawn or not. The
//!   interval between them is how steadily the main thread received vsync. A
//!   50 ms gap at 60 Hz is three lost refreshes. This is *not* present time:
//!   GPUI 0.2.2 exposes no present-completed callback (the Metal renderer
//!   commits the command buffer and returns; see `metal_renderer.rs`), so what
//!   the GPU and compositor did afterwards is not observable from here.
//!   GPUI's own CPU time per drawn frame (`draw` + `present`, including any
//!   wait in `next_drawable`) is available with `ZED_MEASUREMENTS=1`, which
//!   prints `frame duration: ..` to stderr; `probe.sh` sets it and merges the
//!   lines into the report.
//! - **Main-thread work**: named spans around what the view does per event
//!   (`enqueue`, progress, finish, render, the list's row builder), so a
//!   stall can be tied to the work that overlapped it.
//! - **Input to frame** (opt-in scripted driver, `PDFTEXTRACT_PROBE_DRIVE=1`):
//!   synthetic key events dispatched through the window's real key path
//!   (`Window::dispatch_keystroke`) and list scrolls (the scroll offset set
//!   directly: GPUI 0.2.2 cannot be handed a wheel event), timed to
//!   the start of the frame that carries their effect and of the frame after
//!   it (which is when that first frame has been submitted). It skips the
//!   OS event queue, and the frame it reaches is submitted, not scanned out.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use gpui::{App, AppContext, AsyncApp, Keystroke, Window, point, px};
use tpe_app::probe::{self, Kind, Probe};

use super::ShellHandle;

/// Watchdog post to the main thread running it.
pub const EVENT_LOOP: &str =
    "main-thread event-loop latency (watchdog post to foreground task run)";
/// The watchdog thread's own lateness.
pub const WATCHDOG: &str = "control: watchdog thread sleep overshoot (machine-wide scheduling)";
/// Main-thread display-link tick to the next one.
pub const FRAME: &str = "frame interval (display-link ticks handled by the main thread)";
/// Synthetic input handled, to the start of the frame that carries it.
pub const INPUT_TO_FRAME: &str = "scripted input to start of the frame carrying it";
/// Synthetic input handled, to the start of the frame after that.
pub const INPUT_TO_SUBMITTED: &str =
    "scripted input to start of the next frame (previous one submitted)";
/// A real keystroke, after the app handled it, to the next frame.
pub const REAL_KEY_TO_FRAME: &str =
    "real keystroke (observed after handling) to start of next frame";

/// Jobs the batch counter has seen arrive, for naming the phase.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);
/// The driver is dispatching: the passive keystroke observer stays quiet.
static DRIVING: AtomicBool = AtomicBool::new(false);
static LAST_TICK: Mutex<Option<Instant>> = Mutex::new(None);

fn interval_from_env() -> Duration {
    let ms = std::env::var("PDFTEXTRACT_PROBE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms >= 1)
        .unwrap_or(4);
    Duration::from_millis(ms)
}

/// Start the watchdog, the real-keystroke observer and (if asked) the driver.
/// Call once the shell exists.
pub fn start(cx: &mut App) {
    let Some(probe) = probe::get() else {
        return;
    };
    probe.mark("probe started");
    probe.phase("idle: no files yet");
    watchdog(cx, probe);
    cx.observe_keystrokes(move |_, window, _| {
        if DRIVING.load(Ordering::Relaxed) {
            return;
        }
        // Handlers have run; the frame that shows their effect is the next
        // one, so this is the time left until it starts (a lower bound on
        // the real input-to-frame time: the OS queue is before this).
        let sent = Instant::now();
        window.on_next_frame(move |_, _| probe.record(REAL_KEY_TO_FRAME, sent.elapsed()));
    })
    .detach();
    cx.on_app_quit(move |_| async move {
        probe.mark("app quit");
        if let Err(error) = probe.write() {
            eprintln!("probe: cannot write {}: {error}", probe.path().display());
        }
    })
    .detach();
    if std::env::var_os("PDFTEXTRACT_PROBE_DRIVE").is_some_and(|v| v != "0") {
        drive(cx, probe);
    }
}

/// Called when the window opens: remember it and start the frame ticks.
pub fn window_opened(window: &mut Window) {
    if probe::get().is_none() {
        return;
    }
    *LAST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    window.on_next_frame(tick);
}

/// One display-link tick handled by the main thread; asks for the next.
fn tick(window: &mut Window, _cx: &mut App) {
    let Some(probe) = probe::get() else { return };
    let now = Instant::now();
    let previous = LAST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(now);
    if let Some(previous) = previous {
        let gap = now.duration_since(previous);
        if gap < Duration::from_secs(1) {
            probe.record_with(FRAME, gap, Kind::Interval);
        } else {
            // Occluded or minimised windows stop their display link.
            probe.count("frame ticks: gaps over 1 s (not counted as frames)", 1);
        }
    }
    probe.count("frame ticks", 1);
    window.on_next_frame(tick);
}

fn watchdog(cx: &mut App, probe: &'static Probe) {
    let interval = interval_from_env();
    probe.mark(format!("watchdog interval {} ms", interval.as_millis()));
    let (sender, mut receiver) = mpsc::unbounded::<Instant>();
    let spawned = std::thread::Builder::new()
        .name("probe-watchdog".into())
        .spawn(move || {
            let mut next = Instant::now() + interval;
            let mut last_write = Instant::now();
            loop {
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                let woke = Instant::now();
                probe.record(WATCHDOG, woke.saturating_duration_since(next));
                if sender.unbounded_send(woke).is_err() {
                    return;
                }
                next += interval;
                if next < woke {
                    // Do not queue a backlog of posts after being starved.
                    next = woke + interval;
                }
                // A periodic snapshot, so a killed app still leaves a report.
                // Rare, and off the main thread; it costs the watchdog one
                // pass over the samples.
                if last_write.elapsed() > Duration::from_secs(20) {
                    let _ = probe.write();
                    last_write = Instant::now();
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("probe: cannot start the watchdog: {error}");
        return;
    }
    cx.spawn(async move |_cx| {
        while let Some(sent) = receiver.next().await {
            let ran = Instant::now();
            probe.record(EVENT_LOOP, ran.saturating_duration_since(sent));
            // Posts that queued behind a stall run now, each with its own delay.
            while let Some(Some(sent)) = receiver.next().now_or_never() {
                probe.record(EVENT_LOOP, ran.saturating_duration_since(sent));
            }
        }
    })
    .detach();
}

/// Files arrived: name the phase so later samples are grouped under it.
pub fn files_arrived(added: usize, source: &str) {
    let Some(probe) = probe::get() else { return };
    let total = ARRIVED.fetch_add(added, Ordering::Relaxed) + added;
    probe.mark(format!("{added} files arrived ({source}); {total} so far"));
    probe.phase(format!("jobs running: {total} files arrived so far"));
}

/// The queue emptied.
pub fn queue_drained() {
    let Some(probe) = probe::get() else { return };
    let total = ARRIVED.load(Ordering::Relaxed);
    probe.mark("queue drained");
    probe.phase(format!("idle: queue empty after {total} files"));
}

// ---- the scripted driver ----

fn drive(cx: &mut App, probe: &'static Probe) {
    probe.mark("driver: scripted input on");
    cx.spawn(async move |cx| {
        pause(cx, 4000).await;
        let mut cycle = 0usize;
        loop {
            pause(cx, 900).await;
            cycle += 1;
            probe.mark(format!("driver: burst {cycle}"));
            DRIVING.store(true, Ordering::Relaxed);
            focus_list(cx);
            for i in 0..12 {
                key(cx, probe, if i % 2 == 0 { "down" } else { "up" }, true);
                pause(cx, 37).await;
            }
            for i in 0..12 {
                scroll(cx, if (i / 3) % 2 == 0 { -70.0 } else { 70.0 });
                pause(cx, 41).await;
            }
            if cycle.is_multiple_of(3) {
                probe.mark("driver: text size larger");
                key(cx, probe, "cmd-=", false);
                pause(cx, 500).await;
                probe.mark("driver: text size smaller");
                key(cx, probe, "cmd--", false);
                pause(cx, 500).await;
            }
            DRIVING.store(false, Ordering::Relaxed);
        }
    })
    .detach();
}

async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

fn focus_list(cx: &mut AsyncApp) {
    let _ = cx.update(|cx| {
        let Some(shell) = cx.try_global::<ShellHandle>().map(|h| h.0.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let focus = shell.read(cx).list_focus.clone();
        let _ = cx.update_window(window, |_, window, _| window.focus(&focus));
    });
}

/// Dispatch a keystroke through the window. `needs_rows`: the key only has an
/// effect (and so only counts as a sample) when the selection moves.
fn key(cx: &mut AsyncApp, probe: &'static Probe, keystroke: &str, needs_rows: bool) {
    let Ok(keystroke) = Keystroke::parse(keystroke) else {
        return;
    };
    let _ = cx.update(|cx| {
        let Some(shell) = cx.try_global::<ShellHandle>().map(|h| h.0.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let before = shell.read(cx).selected;
        let _ = cx.update_window(window, |_, window, cx| {
            let started = Instant::now();
            window.dispatch_keystroke(keystroke, cx);
            probe.record_with(
                "input: dispatch_keystroke handlers",
                started.elapsed(),
                Kind::Span,
            );
            if needs_rows && shell.read(cx).selected == before {
                return; // no visible effect: nothing to time
            }
            measure_effect(window, started);
        });
    });
}

/// Move the list by `dy` pixels. This sets the scroll offset directly: GPUI
/// 0.2.2 cannot be handed a wheel event from outside (`Window::dispatch_event`
/// returns a private type), so it is the list's redraw with new rows that is
/// timed here, not the wheel's path through the platform.
fn scroll(cx: &mut AsyncApp, dy: f32) {
    let _ = cx.update(|cx| {
        let Some(shell) = cx.try_global::<ShellHandle>().map(|h| h.0.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        // A short list does not scroll: nothing to time.
        if shell.read(cx).jobs.rows().len() < 8 {
            return;
        }
        let handle = shell.read(cx).scroll.0.borrow().base_handle.clone();
        let _ = cx.update_window(window, |_, window, _| {
            let started = Instant::now();
            let old = handle.offset();
            handle.set_offset(point(old.x, old.y + px(dy)));
            window.refresh();
            measure_effect(window, started);
        });
    });
}

/// Time an input's effect: to the start of the next frame (which draws it)
/// and to the start of the one after (by which the first was submitted).
fn measure_effect(window: &mut Window, started: Instant) {
    let Some(probe) = probe::get() else { return };
    window.on_next_frame(move |window, _| {
        probe.record(INPUT_TO_FRAME, started.elapsed());
        window.on_next_frame(move |_, _| probe.record(INPUT_TO_SUBMITTED, started.elapsed()));
    });
}
