//! `PDFTextract`'s window: two buttons that are also drop targets, and a list
//! of jobs with a progress bar each. The model and the engine calls live in
//! `tpe_app::jobs`; this module only wires them to GPUI.
//!
//! Everything the user does is answered synchronously on the main thread
//! (a row appears before the engine is even asked), and the engine runs on
//! GPUI's background executor in this process: no subprocess, no
//! serialisation. Progress events are coalesced per frame.
//!
//! GPUI 0.2.2 items used, verified against the crate source: `Application::on_open_urls`,
//! `App::{prompt_for_paths, reveal_path, write_to_clipboard, set_menus, on_window_closed,
//! windows, background_executor}` (`src/app.rs`), `Context::spawn` (async closure
//! over `WeakEntity`), `InteractiveElement::{on_drop, drag_over}` with
//! `ExternalPaths` (`src/elements/div.rs`, `src/interactive.rs`),
//! `DefiniteLength::Fraction` for the bar (`src/geometry.rs`), `Window::{focus,
//! focus_next, focus_prev}` (`src/window.rs`).
//!
//! # Accessibility
//!
//! GPUI 0.2.2 has no accessibility tree (see the note in the workbench that
//! preceded this window, in the git history of this file), so `VoiceOver`,
//! Voice Control and Switch Control cannot see these controls. What this
//! window does provide: every action on a key (⌘O, ⌘B, Tab/Shift-Tab, Enter
//! or Space on the focused button, ⌘K, ⌘Q), the File menu, large targets,
//! nothing timed, and visible text on every control.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use futures::channel::mpsc::{self, UnboundedSender};
use futures::{FutureExt, StreamExt};
use gpui::{
    App, Application, Bounds, ClickEvent, ClipboardItem, Context, DefiniteLength, Div,
    ExternalPaths, FocusHandle, FontWeight, KeyBinding, Menu, MenuItem, PathPromptOptions,
    Stateful, SystemMenuType, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, rgb, size,
};

use tpe::pipeline::Progress;
use tpe_app::jobs::{self, Action, JobList, JobRow, Phase};

actions!(
    pdftextract,
    [
        Quit,
        GetText,
        GetBibliography,
        ClearDone,
        Activate,
        FocusNext,
        FocusPrev
    ]
);

const BG: u32 = 0x0018_1a1f;
const PANEL: u32 = 0x0020_232a;
const BORDER: u32 = 0x003a_3f4a;
const ACCENT: u32 = 0x0058_a6ff;
const DROP: u32 = 0x002a_4a6e;
const TEXT: u32 = 0x00e6_e6e6;
const MUTED: u32 = 0x009a_a0aa;
const FAILED: u32 = 0x00ff_7b72;
const BUTTON: u32 = 0x002f_3440;
const BUTTON_HOVER: u32 = 0x003d_4454;
const TRACK: u32 = 0x002f_3440;

/// Paths handed to the running app from outside the view: Finder Services
/// (`services.rs`) and files opened with the app (`App::on_open_urls`).
type Intake = (Action, Vec<PathBuf>);
static INTAKE: OnceLock<UnboundedSender<Intake>> = OnceLock::new();

/// Queue `paths` for `action` in the running window. Safe from any thread;
/// dropped silently before the window exists.
pub fn intake(action: Action, paths: Vec<PathBuf>) {
    if let Some(sender) = INTAKE.get() {
        let _ = sender.unbounded_send((action, paths));
    }
}

/// The window's view.
pub struct Shell {
    jobs: JobList,
    ledger: PathBuf,
    root_focus: FocusHandle,
    text_focus: FocusHandle,
    biblio_focus: FocusHandle,
}

impl Shell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let biblio_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        window.focus(&text_focus);

        // Paths from Finder or the command line arrive on this channel.
        let (sender, mut receiver) = mpsc::unbounded::<Intake>();
        let _ = INTAKE.set(sender);
        cx.spawn(async move |this, cx| {
            while let Some((action, paths)) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.enqueue(paths, action, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Self {
            jobs: JobList::default(),
            ledger: jobs::default_ledger_path(),
            root_focus: cx.focus_handle(),
            text_focus,
            biblio_focus,
        }
    }

    /// Add rows and start work if idle. The rows show before the engine runs.
    fn enqueue(&mut self, paths: Vec<PathBuf>, action: Action, cx: &mut Context<Self>) {
        if self.jobs.enqueue(paths, action) > 0 {
            cx.notify();
            self.pump(cx);
        }
    }

    /// The system open panel, PDFs many at once.
    fn choose(action: Action, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(action.title().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                let _ = this.update(cx, |this, cx| this.enqueue(paths, action, cx));
            }
        })
        .detach();
    }

    /// Run the oldest queued job on the background executor, one at a time.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.jobs.next_queued() else {
            return;
        };
        let Some(row) = self.jobs.row(id) else {
            return;
        };
        let action = row.action;
        let source = row.source.clone();
        let ledger = self.ledger.clone();
        self.jobs.start(id);
        cx.notify();

        let (sender, mut receiver) = mpsc::unbounded::<Progress>();
        let task = cx.background_executor().spawn(async move {
            jobs::run(action, &source, &ledger, &mut |event| {
                let _ = sender.unbounded_send(event);
            })
        });
        // Progress: keep only the newest event waiting at each frame.
        cx.spawn(async move |this, cx| {
            while let Some(mut event) = receiver.next().await {
                while let Some(Some(later)) = receiver.next().now_or_never() {
                    event = later;
                }
                let applied = this.update(cx, |this, cx| {
                    this.jobs.progress(id, event);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.jobs.finish(id, result);
                cx.notify();
                this.pump(cx);
            });
        })
        .detach();
    }

    fn copy(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(path) = self.jobs.row(id).and_then(JobRow::copyable) else {
            return;
        };
        if let Ok(text) = fs::read_to_string(path) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn reveal(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(row) = self.jobs.row(id) else {
            return;
        };
        let path = row.outputs().first().unwrap_or(&row.source);
        cx.reveal_path(path);
    }

    fn remove(&mut self, id: usize, cx: &mut Context<Self>) {
        if self.jobs.remove(id) {
            cx.notify();
        }
    }

    #[allow(clippy::unused_self)]
    fn on_get_text(&mut self, _: &GetText, _: &mut Window, cx: &mut Context<Self>) {
        Self::choose(Action::Text, cx);
    }

    #[allow(clippy::unused_self)]
    fn on_get_bibliography(&mut self, _: &GetBibliography, _: &mut Window, cx: &mut Context<Self>) {
        Self::choose(Action::Bibliography, cx);
    }

    fn on_clear_done(&mut self, _: &ClearDone, _: &mut Window, cx: &mut Context<Self>) {
        self.jobs.clear_done();
        cx.notify();
    }

    /// Enter or Space on a focused button.
    fn on_activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_focus.is_focused(window) {
            Self::choose(Action::Text, cx);
        } else if self.biblio_focus.is_focused(window) {
            Self::choose(Action::Bibliography, cx);
        }
    }

    // GPUI listeners take `&mut Self` even when only the window moves.
    #[allow(clippy::unused_self)]
    fn on_focus_next(&mut self, _: &FocusNext, window: &mut Window, _: &mut Context<Self>) {
        window.focus_next();
    }

    #[allow(clippy::unused_self)]
    fn on_focus_prev(&mut self, _: &FocusPrev, window: &mut Window, _: &mut Context<Self>) {
        window.focus_prev();
    }

    /// A large button that is also a drop target for PDF files.
    fn action_button(
        action: Action,
        focus: &FocusHandle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id: &'static str = match action {
            Action::Text => "get-text",
            Action::Bibliography => "get-bibliography",
        };
        let focused = focus.is_focused(window);
        div()
            .id(id)
            .track_focus(focus)
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .min_h(px(132.0))
            .rounded_md()
            .border_2()
            .border_color(rgb(if focused { ACCENT } else { BORDER }))
            .bg(rgb(BUTTON))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(BUTTON_HOVER)))
            .drag_over::<ExternalPaths>(|style, _, _, _| {
                style.bg(rgb(DROP)).border_color(rgb(ACCENT))
            })
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.enqueue(paths.paths().to_vec(), action, cx);
            }))
            .on_click(cx.listener(move |_this, _: &ClickEvent, _, cx| {
                Self::choose(action, cx);
            }))
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(action.title()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("Drop PDFs here"),
            )
    }

    fn render_row(row: &JobRow, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = row.id;
        let status_color = match row.phase {
            Phase::Failed(_) => FAILED,
            _ => MUTED,
        };
        let mut buttons = div().flex().gap_2().flex_shrink_0();
        match row.phase {
            Phase::Queued => {
                buttons = buttons.child(small_button(
                    ("remove", id),
                    "Remove",
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.remove(id, cx)),
                ));
            }
            Phase::Running => {}
            Phase::Finished(_) | Phase::Failed(_) => {
                if row.copyable().is_some() {
                    buttons = buttons.child(small_button(
                        ("copy", id),
                        "Copy",
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.copy(id, cx)),
                    ));
                }
                buttons = buttons.child(small_button(
                    ("reveal", id),
                    "Show in Finder",
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.reveal(id, cx)),
                ));
            }
        }
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_1()
            .overflow_hidden()
            .child(div().text_lg().truncate().child(row.name()));
        if row.phase == Phase::Running {
            let fraction = row.fraction().unwrap_or(0.0);
            column = column.child(
                div().w_full().h(px(6.0)).rounded_md().bg(rgb(TRACK)).child(
                    div()
                        .h_full()
                        .rounded_md()
                        .bg(rgb(ACCENT))
                        .w(DefiniteLength::Fraction(fraction)),
                ),
            );
        }
        column = column.child(
            div()
                .text_sm()
                .text_color(rgb(status_color))
                .child(row.status_line()),
        );
        div()
            .id(("job", id))
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(PANEL))
            .child(column)
            .child(buttons)
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<Stateful<Div>> = self
            .jobs
            .rows()
            .iter()
            .map(|row| Self::render_row(row, cx))
            .collect();
        let empty = rows.is_empty();
        let has_done = self.jobs.has_done();
        div()
            .id("shell")
            .track_focus(&self.root_focus)
            .key_context("Shell")
            .on_action(cx.listener(Self::on_get_text))
            .on_action(cx.listener(Self::on_get_bibliography))
            .on_action(cx.listener(Self::on_clear_done))
            .on_action(cx.listener(Self::on_activate))
            .on_action(cx.listener(Self::on_focus_next))
            .on_action(cx.listener(Self::on_focus_prev))
            .flex()
            .flex_col()
            .size_full()
            .gap_4()
            .p_5()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(Self::action_button(Action::Text, &self.text_focus.clone(), window, cx))
                    .child(Self::action_button(
                        Action::Bibliography,
                        &self.biblio_focus.clone(),
                        window,
                        cx,
                    )),
            )
            .when(empty, |this| {
                this.child(div().text_sm().text_color(rgb(MUTED)).child(
                    "Drop PDFs on a button, or press it to choose files. Output lands next to each PDF.",
                ))
            })
            .when(!empty, |this| {
                this.child(
                    div()
                        .id("jobs")
                        .flex_1()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(rows),
                )
            })
            .when(has_done, |this| {
                this.child(
                    div().flex().child(small_button(
                        ("clear", 0),
                        "Clear finished (⌘K)",
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.jobs.clear_done();
                            cx.notify();
                        }),
                    )),
                )
            })
    }
}

/// A labelled row button.
fn small_button(
    id: (&'static str, usize),
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BUTTON))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BUTTON_HOVER)))
        .child(label)
        .on_click(on_click)
}

/// Start the app: keys, menus, the Finder hooks, and the window. `paths`
/// (from the command line) are queued for text extraction once it is open.
pub fn run(paths: Vec<PathBuf>) {
    let app = Application::new();
    // Files opened with the app (Open With, a drop on the Dock icon): there
    // is no way to say which action, so text is the default.
    app.on_open_urls(|urls| {
        let paths: Vec<PathBuf> = urls
            .iter()
            .filter_map(|url| jobs::file_url_to_path(url))
            .collect();
        intake(Action::Text, paths);
    });
    app.run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-o", GetText, Some("Shell")),
            KeyBinding::new("cmd-b", GetBibliography, Some("Shell")),
            KeyBinding::new("cmd-k", ClearDone, Some("Shell")),
            KeyBinding::new("enter", Activate, Some("Shell")),
            KeyBinding::new("space", Activate, Some("Shell")),
            KeyBinding::new("tab", FocusNext, Some("Shell")),
            KeyBinding::new("shift-tab", FocusPrev, Some("Shell")),
        ]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "PDFTextract".into(),
                items: vec![
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action("Quit PDFTextract", Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Get Text…", GetText),
                    MenuItem::action("Get Bibliography…", GetBibliography),
                    MenuItem::separator(),
                    MenuItem::action("Clear Finished", ClearDone),
                ],
            },
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // Compile the engine's regexes now, not inside the first job.
        cx.background_executor()
            .spawn(async { tpe::pipeline::warm_up() })
            .detach();

        let bounds = Bounds::centered(None, size(px(640.0), px(480.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(480.0), px(320.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("PDFTextract".into()),
                ..TitlebarOptions::default()
            }),
            ..WindowOptions::default()
        };
        match cx.open_window(options, |window, cx| cx.new(|cx| Shell::new(window, cx))) {
            Ok(_) => {
                cx.activate(true);
                #[cfg(target_os = "macos")]
                crate::services::install();
                if !paths.is_empty() {
                    intake(Action::Text, paths);
                }
            }
            Err(error) => {
                eprintln!("cannot open window: {error}");
                cx.quit();
            }
        }
    });
}
