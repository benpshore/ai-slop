//! `PDFTextract`'s window: two buttons that are also drop targets, and a list
//! of jobs with a progress bar each. The model and the engine calls live in
//! `tpe_app::jobs`; this module only wires them to GPUI.
//!
//! Everything the user does is answered synchronously on the main thread
//! (a row appears before the engine is even asked), and the engine runs on
//! GPUI's background executor in this process: no subprocess, no
//! serialisation. Progress events are coalesced per frame.
//!
//! The view outlives its window: it is held as a GPUI global, so closing the
//! window while jobs are queued or running lets them finish (the app quits
//! itself once idle with no window), and the Dock icon reopens the window on
//! the same state.
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
//! window does provide: every action on a key (⌘O, ⌘B, ⌘K, ⌘Q; Tab/Shift-Tab
//! through the two big buttons, every row's buttons and Clear finished; Enter
//! or Space on the focused one), the File menu, large targets, nothing timed,
//! and visible text on every control.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt};
use gpui::{
    App, Application, Bounds, ClickEvent, ClipboardItem, Context, DefiniteLength, Div, Entity,
    ExternalPaths, FocusHandle, FontWeight, Global, KeyBinding, Menu, MenuItem, PathPromptOptions,
    Stateful, SystemMenuType, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, rgb, size,
};

use tpe::pipeline::Progress;
use tpe_app::jobs::{self, Action, JobList, JobRow, Mailbox, Phase};

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

/// Paths handed to the app from outside the view: Finder Services
/// (`services.rs`), files opened with the app (`Application::on_open_urls`,
/// which macOS may call before the window exists) and the command line.
type Intake = (Action, Vec<PathBuf>);
static INTAKE: Mailbox<Intake> = Mailbox::new();

/// Queue `paths` for `action`. Safe from any thread and at any time: items
/// sent before the window exists are delivered once it does.
pub fn intake(action: Action, paths: Vec<PathBuf>) {
    INTAKE.send((action, paths));
}

/// The view, kept alive independently of its window.
struct ShellHandle(Entity<Shell>);

impl Global for ShellHandle {}

/// Focus handles of one row's buttons, so each is a tab stop.
struct RowFocus {
    remove: FocusHandle,
    copy: FocusHandle,
    reveal: FocusHandle,
}

/// Tab indices: the two big buttons, then `ROW_TAB_BASE + 4 * id + k` for
/// row `id`'s buttons, then Clear finished last.
const ROW_TAB_BASE: isize = 10;
const CLEAR_TAB_INDEX: isize = isize::MAX / 2;

/// Shows a path in Finder.
type Reveal = Rc<dyn Fn(&mut App, &Path)>;

/// Opens a file chooser and delivers the chosen paths (`None`: cancelled).
type Picker = Rc<
    dyn Fn(&mut App, PathPromptOptions) -> oneshot::Receiver<gpui::Result<Option<Vec<PathBuf>>>>,
>;

/// The platform calls the view makes, behind one seam so the headless tests
/// can answer them (GPUI's test platform has no file chooser, no Finder, and
/// a `quit` that does nothing observable).
struct Host {
    pick: Picker,
    reveal: Reveal,
    quit: Rc<dyn Fn(&mut App)>,
}

impl Host {
    /// The system open panel, Finder, and quitting the application.
    fn system() -> Self {
        Self {
            pick: Rc::new(|cx, options| cx.prompt_for_paths(options)),
            reveal: Rc::new(|cx, path| cx.reveal_path(path)),
            quit: Rc::new(|cx| cx.quit()),
        }
    }
}

/// The window's view.
pub struct Shell {
    jobs: JobList,
    ledger: PathBuf,
    host: Host,
    root_focus: FocusHandle,
    text_focus: FocusHandle,
    biblio_focus: FocusHandle,
    clear_focus: FocusHandle,
    row_focus: HashMap<usize, RowFocus>,
}

impl Shell {
    /// `intake` delivers paths from outside the view (Finder, the command
    /// line), `ledger` is where Get text records its runs, `host` answers
    /// the platform calls.
    fn new(intake: &Mailbox<Intake>, ledger: PathBuf, host: Host, cx: &mut Context<Self>) -> Self {
        let text_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let biblio_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        let clear_focus = cx.focus_handle().tab_index(CLEAR_TAB_INDEX).tab_stop(true);

        // Paths from Finder or the command line arrive on this channel,
        // including any that arrived before this view existed.
        let (sender, mut receiver) = mpsc::unbounded::<Intake>();
        intake.install(sender);
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
            ledger,
            host,
            root_focus: cx.focus_handle(),
            text_focus,
            biblio_focus,
            clear_focus,
            row_focus: HashMap::new(),
        }
    }

    /// The focus handles of row `id`'s buttons, created on first use.
    fn row_focus(&mut self, id: usize, cx: &mut Context<Self>) -> &RowFocus {
        self.row_focus.entry(id).or_insert_with(|| {
            let base = ROW_TAB_BASE + 4 * isize::try_from(id).unwrap_or(isize::MAX / 8);
            RowFocus {
                remove: cx.focus_handle().tab_index(base).tab_stop(true),
                copy: cx.focus_handle().tab_index(base + 1).tab_stop(true),
                reveal: cx.focus_handle().tab_index(base + 2).tab_stop(true),
            }
        })
    }

    /// Drop focus handles of rows that no longer exist.
    fn prune_row_focus(&mut self) {
        let live: Vec<usize> = self.jobs.rows().iter().map(|row| row.id).collect();
        self.row_focus.retain(|id, _| live.contains(id));
    }

    /// Quit once nothing is queued or running and no window is open.
    fn quit_if_idle(&self, cx: &mut Context<Self>) {
        if cx.windows().is_empty() && !self.jobs.has_active() {
            let quit = self.host.quit.clone();
            quit(cx);
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
    fn choose(&self, action: Action, cx: &mut Context<Self>) {
        let receiver = (self.host.pick)(
            cx,
            PathPromptOptions {
                files: true,
                directories: false,
                multiple: true,
                prompt: Some(action.title().into()),
            },
        );
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
                this.quit_if_idle(cx);
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
        let path = row.outputs().first().unwrap_or(&row.source).clone();
        (self.host.reveal)(cx, &path);
    }

    fn remove(&mut self, id: usize, cx: &mut Context<Self>) {
        if self.jobs.remove(id) {
            self.prune_row_focus();
            cx.notify();
        }
    }

    fn clear_done(&mut self, cx: &mut Context<Self>) {
        self.jobs.clear_done();
        self.prune_row_focus();
        cx.notify();
    }

    fn on_get_text(&mut self, _: &GetText, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(Action::Text, cx);
    }

    fn on_get_bibliography(&mut self, _: &GetBibliography, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(Action::Bibliography, cx);
    }

    fn on_clear_done(&mut self, _: &ClearDone, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_done(cx);
    }

    /// Enter or Space on whichever button has focus.
    fn on_activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_focus.is_focused(window) {
            self.choose(Action::Text, cx);
        } else if self.biblio_focus.is_focused(window) {
            self.choose(Action::Bibliography, cx);
        } else if self.clear_focus.is_focused(window) {
            self.clear_done(cx);
        } else if let Some((id, which)) = self.focused_row_button(window) {
            match which {
                RowButton::Remove => self.remove(id, cx),
                RowButton::Copy => self.copy(id, cx),
                RowButton::Reveal => self.reveal(id, cx),
            }
        }
    }

    fn focused_row_button(&self, window: &Window) -> Option<(usize, RowButton)> {
        self.row_focus.iter().find_map(|(id, focus)| {
            if focus.remove.is_focused(window) {
                Some((*id, RowButton::Remove))
            } else if focus.copy.is_focused(window) {
                Some((*id, RowButton::Copy))
            } else if focus.reveal.is_focused(window) {
                Some((*id, RowButton::Reveal))
            } else {
                None
            }
        })
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
            .debug_selector(|| id.to_string())
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
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.choose(action, cx);
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

    fn render_row(&mut self, row: &JobRow, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = row.id;
        let status_color = match row.phase {
            Phase::Failed(_) => FAILED,
            _ => MUTED,
        };
        let focus = self.row_focus(id, cx);
        let (remove_focus, copy_focus, reveal_focus) = (
            focus.remove.clone(),
            focus.copy.clone(),
            focus.reveal.clone(),
        );
        let mut buttons = div().flex().gap_2().flex_shrink_0();
        match row.phase {
            Phase::Queued => {
                buttons = buttons.child(small_button(
                    ("remove", id),
                    "Remove",
                    &remove_focus,
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.remove(id, cx)),
                ));
            }
            Phase::Running => {}
            Phase::Finished(_) | Phase::Failed(_) => {
                if row.copyable().is_some() {
                    buttons = buttons.child(small_button(
                        ("copy", id),
                        "Copy",
                        &copy_focus,
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.copy(id, cx)),
                    ));
                }
                buttons = buttons.child(small_button(
                    ("reveal", id),
                    "Show in Finder",
                    &reveal_focus,
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
        let snapshot: Vec<JobRow> = self.jobs.rows().to_vec();
        let rows: Vec<Stateful<Div>> = snapshot
            .iter()
            .map(|row| self.render_row(row, cx))
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
                        &self.clear_focus,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.clear_done(cx)),
                    )),
                )
            })
    }
}

/// Which of a row's buttons has focus.
#[derive(Clone, Copy)]
enum RowButton {
    Remove,
    Copy,
    Reveal,
}

/// A labelled button that is a tab stop (`focus`) and shows an accent
/// border while focused; Enter and Space reach it through `Activate`.
fn small_button(
    id: (&'static str, usize),
    label: &'static str,
    focus: &FocusHandle,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(focus)
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BUTTON))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BUTTON_HOVER)))
        .focus(|style| style.border_color(rgb(ACCENT)))
        .child(label)
        .on_click(on_click)
}

/// Open the window on the shared view, focusing the first button.
fn open_main_window(cx: &mut App) {
    let Some(shell) = cx
        .try_global::<ShellHandle>()
        .map(|handle| handle.0.clone())
    else {
        return;
    };
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
    match cx.open_window(options, |window, cx| {
        let focus = shell.read(cx).text_focus.clone();
        window.focus(&focus);
        shell.clone()
    }) {
        Ok(_) => cx.activate(true),
        Err(error) => {
            eprintln!("cannot open window: {error}");
            cx.quit();
        }
    }
}

/// Bind the keys and the application-wide behaviour: the shortcuts, Quit,
/// and "closing the window is not quitting while work is queued or running"
/// (the view lives on as a global and quits once idle). Shared by [`run`]
/// and the headless tests.
fn setup(cx: &mut App) {
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
    cx.on_window_closed(|cx| {
        let Some(shell) = cx
            .try_global::<ShellHandle>()
            .map(|handle| handle.0.clone())
        else {
            if cx.windows().is_empty() {
                cx.quit();
            }
            return;
        };
        let (idle, quit) = {
            let shell = shell.read(cx);
            (!shell.jobs.has_active(), shell.host.quit.clone())
        };
        if cx.windows().is_empty() && idle {
            quit(cx);
        }
    })
    .detach();
}

/// The menu bar: the app menu with Services, and File.
fn set_menus(cx: &mut App) {
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
}

/// Start the app: keys, menus, the Finder hooks, and the window. `paths`
/// (from the command line) are queued for text extraction once it is open.
pub fn run(paths: Vec<PathBuf>) {
    let app = Application::new();
    // Files opened with the app (Open With, a drop on the Dock icon): there
    // is no way to say which action, so text is the default. At launch
    // these arrive before `run`'s callback; the mailbox keeps them.
    app.on_open_urls(|urls| {
        let paths: Vec<PathBuf> = urls
            .iter()
            .filter_map(|url| jobs::file_url_to_path(url))
            .collect();
        intake(Action::Text, paths);
    });
    // The Dock icon, once the window was closed with jobs still running.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() && cx.has_global::<ShellHandle>() {
            open_main_window(cx);
        }
    });
    app.run(move |cx: &mut App| {
        setup(cx);
        set_menus(cx);
        // Compile the engine's regexes now, not inside the first job.
        cx.background_executor()
            .spawn(async { tpe::pipeline::warm_up() })
            .detach();

        let shell =
            cx.new(|cx| Shell::new(&INTAKE, jobs::default_ledger_path(), Host::system(), cx));
        cx.set_global(ShellHandle(shell));
        open_main_window(cx);
        crate::services::install();
        if !paths.is_empty() {
            intake(Action::Text, paths);
        }
    });
}

#[cfg(test)]
mod tests;
