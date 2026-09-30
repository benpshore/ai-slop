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

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt};
use gpui::{
    App, Application, Bounds, ClickEvent, ClipboardItem, Context, DefiniteLength, Div, Entity,
    ExternalPaths, FocusHandle, FontWeight, Global, KeyBinding, Menu, MenuItem, PathPromptOptions,
    ScrollStrategy, Stateful, SystemMenuType, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rems, rgb, size, uniform_list,
};

use tpe::pipeline::Progress;
use tpe_app::jobs::{self, Action, CancelToken, JobList, JobRow, Mailbox, Phase, Step};
use tpe_app::view::TextScale;

actions!(
    pdftextract,
    [
        Quit,
        GetText,
        GetBibliography,
        ClearDone,
        Activate,
        FocusNext,
        FocusPrev,
        SelectPrev,
        SelectNext,
        SelectFirst,
        SelectLast,
        CopyText,
        RevealSelected,
        RemoveSelected,
        CancelSelected,
        TextLarger,
        TextSmaller,
        TextReset
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

/// Tab order: Get text, Get bibliography, the job list (one stop; the arrow
/// keys move a selection through its rows), then Clear finished.
const LIST_TAB_INDEX: isize = 3;
const CLEAR_TAB_INDEX: isize = 4;

/// Height of one job row, in rems so it follows the text size. Rows are all
/// this tall, which is what lets the list draw only the ones on screen. It
/// holds the worst case: a title, the bar slot and a status wrapped to two
/// lines (about 5.8 rem with the row's border and padding).
const ROW_HEIGHT_REMS: f32 = 6.5;

/// The smallest window (points): the action header, one whole row and the
/// Clear finished button still fit at the default text size.
const MIN_WINDOW: (f32, f32) = (480.0, 400.0);

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
    list_focus: FocusHandle,
    clear_focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// The selected row's id.
    selected: Option<usize>,
    /// The running job and the flag that stops it.
    running: Option<(usize, Arc<CancelToken>)>,
    /// The text size; stored beside the ledger so it survives a relaunch.
    text_scale: TextScale,
    scale_file: PathBuf,
}

impl Shell {
    /// `intake` delivers paths from outside the view (Finder, the command
    /// line), `ledger` is where Get text records its runs, `host` answers
    /// the platform calls.
    fn new(intake: &Mailbox<Intake>, ledger: PathBuf, host: Host, cx: &mut Context<Self>) -> Self {
        let scale_file = ledger.with_file_name("text-scale");
        let text_scale = TextScale::load(&scale_file);
        let text_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let biblio_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        let list_focus = cx.focus_handle().tab_index(LIST_TAB_INDEX).tab_stop(true);
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
            list_focus,
            clear_focus,
            scroll: UniformListScrollHandle::new(),
            selected: None,
            running: None,
            text_scale,
            scale_file,
        }
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

        let cancel = Arc::new(CancelToken::new());
        self.running = Some((id, cancel.clone()));
        let (sender, mut receiver) = mpsc::unbounded::<Progress>();
        let task = cx.background_executor().spawn(async move {
            jobs::run(
                action,
                &source,
                &ledger,
                &mut |event| {
                    let _ = sender.unbounded_send(event);
                },
                &cancel,
            )
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
                this.running = None;
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
        let index = self.jobs.index_of(id);
        if self.jobs.remove(id) {
            if self.selected == Some(id) {
                self.selected = index.and_then(|index| self.jobs.nearest_to(index));
            }
            cx.notify();
        }
    }

    /// Stop running job `id` at its next page. Nothing is written for it. A
    /// job that has already begun writing its results is not stopped: the
    /// request is refused and the row stays as it is until it finishes.
    fn cancel(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some((running, token)) = &self.running else {
            return;
        };
        if *running == id && token.request() && self.jobs.mark_cancelling(id) {
            cx.notify();
        }
    }

    fn clear_done(&mut self, cx: &mut Context<Self>) {
        // Rows that will survive, ahead of the selected one: the first
        // survivor after a removed selection lands at exactly that index.
        let survivors_before = self
            .selected
            .and_then(|id| self.jobs.index_of(id))
            .map(|at| {
                self.jobs.rows()[..at]
                    .iter()
                    .filter(|row| row.is_active())
                    .count()
            });
        self.jobs.clear_done();
        if self.selected.is_some_and(|id| self.jobs.row(id).is_none()) {
            self.selected = self.jobs.nearest_to(survivors_before.unwrap_or(0));
        }
        // The list is shorter and its rows have new indices: the old scroll
        // offset would point past them or away from the selection.
        let at = self
            .selected
            .and_then(|id| self.jobs.index_of(id))
            .unwrap_or(0);
        self.scroll.scroll_to_item(at, ScrollStrategy::Top);
        cx.notify();
    }

    /// Select row `id`.
    fn select(&mut self, id: usize, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.notify();
    }

    /// Move the selection and scroll just enough to keep it fully visible.
    fn step_selection(&mut self, step: Step, window: &Window, cx: &mut Context<Self>) {
        if let Some(id) = self.jobs.stepped(self.selected, step) {
            self.select(id, cx);
            if let Some(index) = self.jobs.index_of(id) {
                self.keep_visible(index, window);
            }
        }
    }

    /// Scroll the list only when row `index` is not fully on screen: up to
    /// put it at the top, down to put it at the bottom.
    fn keep_visible(&self, index: usize, window: &Window) {
        let state = self.scroll.0.borrow();
        let Some(size) = state.last_item_size else {
            return; // not laid out yet: the list is at the top
        };
        let row = f32::from(window.rem_size()) * ROW_HEIGHT_REMS;
        let top = -f32::from(state.base_handle.offset().y);
        let first_visible = (top / row).ceil();
        let last_visible = ((top + f32::from(size.item.height)) / row).floor() - 1.0;
        drop(state);
        #[allow(clippy::cast_precision_loss)]
        let at = index as f32;
        if at < first_visible {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        } else if at > last_visible {
            self.scroll.scroll_to_item(index, ScrollStrategy::Bottom);
        }
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

    /// Enter or Space on whichever button or list has focus: the two big
    /// buttons choose files, Clear finished clears, the list shows the
    /// selected row's result in Finder.
    fn on_activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_focus.is_focused(window) {
            self.choose(Action::Text, cx);
        } else if self.biblio_focus.is_focused(window) {
            self.choose(Action::Bibliography, cx);
        } else if self.clear_focus.is_focused(window) {
            self.clear_done(cx);
        } else if self.list_focus.is_focused(window) {
            self.reveal_selected(cx);
        }
    }

    /// Show the selected row's result in Finder. Queued and running rows have
    /// no result yet (and no button), so the key does nothing on them rather
    /// than opening their input PDF.
    fn reveal_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected
            && self.jobs.row(id).is_some_and(|row| !row.is_active())
        {
            self.reveal(id, cx);
        }
    }

    fn on_reveal_selected(&mut self, _: &RevealSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.reveal_selected(cx);
    }

    fn on_copy_text(&mut self, _: &CopyText, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            self.copy(id, cx);
        }
    }

    /// Delete on the selected row: a queued row leaves the list and a running
    /// one is stopped; a finished, failed or cancelled one stays (Clear
    /// finished removes those), so a stray key cannot lose a result.
    fn on_remove_selected(&mut self, _: &RemoveSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        match self.jobs.row(id).map(|row| &row.phase) {
            Some(Phase::Queued) => self.remove(id, cx),
            Some(Phase::Running) => self.cancel(id, cx),
            _ => {}
        }
    }

    /// Escape on the selected row: stop it if it is running (never removes).
    fn on_cancel_selected(&mut self, _: &CancelSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            self.cancel(id, cx);
        }
    }

    fn on_select_prev(&mut self, _: &SelectPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(Step::Up, window, cx);
    }

    fn on_select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(Step::Down, window, cx);
    }

    fn on_select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(Step::First, window, cx);
    }

    fn on_select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(Step::Last, window, cx);
    }

    /// Change the text size, apply it to this window and remember it.
    fn set_text_scale(&mut self, scale: TextScale, window: &mut Window, cx: &mut Context<Self>) {
        self.text_scale = scale;
        window.set_rem_size(px(scale.rem_px()));
        // A file that cannot be written just means the size is not kept.
        let _ = scale.save(&self.scale_file);
        cx.notify();
    }

    fn on_text_larger(&mut self, _: &TextLarger, window: &mut Window, cx: &mut Context<Self>) {
        self.set_text_scale(self.text_scale.larger(), window, cx);
    }

    fn on_text_smaller(&mut self, _: &TextSmaller, window: &mut Window, cx: &mut Context<Self>) {
        self.set_text_scale(self.text_scale.smaller(), window, cx);
    }

    fn on_text_reset(&mut self, _: &TextReset, window: &mut Window, cx: &mut Context<Self>) {
        self.set_text_scale(TextScale::default(), window, cx);
    }

    fn on_focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next();
        self.select_first_if_landed_on_list(window, cx);
    }

    fn on_focus_prev(&mut self, _: &FocusPrev, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev();
        self.select_first_if_landed_on_list(window, cx);
    }

    /// Tabbing into the list selects the first row when nothing is selected,
    /// and brings an existing selection into view (it may have been scrolled
    /// away with the trackpad), so the focus is never on an invisible
    /// selection.
    fn select_first_if_landed_on_list(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.list_focus.is_focused(window) {
            return;
        }
        match self.selected.and_then(|id| self.jobs.index_of(id)) {
            Some(index) => {
                self.keep_visible(index, window);
                cx.notify();
            }
            None => self.step_selection(Step::First, window, cx),
        }
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
            .min_h(rems(8.25))
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

    /// The pointer buttons of one row, by phase; the same actions are on keys
    /// for the selected row.
    fn row_buttons(row: &JobRow, cx: &mut Context<Self>) -> Div {
        let id = row.id;
        let mut buttons = div().flex().gap_2().flex_shrink_0();
        match row.phase {
            Phase::Queued => {
                buttons = buttons.child(small_button(
                    ("remove", id),
                    "Remove",
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.remove(id, cx)),
                ));
            }
            Phase::Running if !row.cancelling => {
                buttons = buttons.child(small_button(
                    ("cancel", id),
                    "Cancel",
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.cancel(id, cx)),
                ));
            }
            Phase::Running => {}
            Phase::Cancelled => {
                buttons = buttons.child(small_button(
                    ("reveal", id),
                    "Show in Finder",
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.reveal(id, cx)),
                ));
            }
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
        buttons
    }

    /// One row: name, a bar slot (drawn only while running, but always
    /// reserved), and a two-line status; the buttons are for the pointer,
    /// the same actions are on keys for the selected row.
    fn render_row(
        row: &JobRow,
        selected: bool,
        list_focused: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = row.id;
        let status_color = match row.phase {
            Phase::Failed(_) => FAILED,
            _ => MUTED,
        };
        let buttons = Self::row_buttons(row, cx);
        let mut bar = div().w_full().h(rems(0.375)).rounded_md();
        if row.phase == Phase::Running {
            bar = bar.bg(rgb(TRACK)).child(
                div()
                    .h_full()
                    .rounded_md()
                    .bg(rgb(ACCENT))
                    .w(DefiniteLength::Fraction(row.fraction().unwrap_or(0.0))),
            );
        }
        let border = match (selected, list_focused) {
            (true, true) => ACCENT,
            (true, false) => MUTED,
            (false, _) => PANEL,
        };
        let column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_1()
            .overflow_hidden()
            .child(
                div()
                    .debug_selector(|| format!("job-{id}-title"))
                    .text_lg()
                    .truncate()
                    .child(row.name()),
            )
            .child(bar)
            .child(
                div()
                    .debug_selector(|| format!("job-{id}-status"))
                    .text_sm()
                    .line_clamp(2)
                    .text_color(rgb(status_color))
                    .child(row.status_line()),
            );
        // The outer element is the list item (fixed height, with the gap
        // between rows as padding); the inner one is the visible panel.
        div()
            .id(("job", id))
            .h(rems(ROW_HEIGHT_REMS))
            .flex_shrink_0()
            .py_1()
            .child(
                div()
                    .debug_selector(|| format!("job-{id}"))
                    .flex()
                    .items_center()
                    .gap_4()
                    .size_full()
                    .px_3()
                    .rounded_md()
                    .border_2()
                    .border_color(rgb(border))
                    .bg(rgb(PANEL))
                    .overflow_hidden()
                    .on_mouse_down(gpui::MouseButton::Left, {
                        let listener = cx.listener(move |this, _, window: &mut Window, cx| {
                            this.select(id, cx);
                            window.focus(&this.list_focus);
                        });
                        move |event, window, cx| listener(event, window, cx)
                    })
                    .child(column)
                    .child(buttons),
            )
    }

    /// The job list: one tab stop, only the rows on screen are built.
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("jobs")
            .track_focus(&self.list_focus)
            .key_context("Jobs")
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "job-rows",
                    self.jobs.rows().len(),
                    cx.processor(|this, range: Range<usize>, window, cx| {
                        let focused = this.list_focus.is_focused(window);
                        let selected = this.selected;
                        let rows: Vec<JobRow> =
                            this.jobs.rows().get(range).unwrap_or_default().to_vec();
                        rows.iter()
                            .map(|row| Self::render_row(row, selected == Some(row.id), focused, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .size_full(),
            )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.jobs.rows().is_empty();
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
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_first))
            .on_action(cx.listener(Self::on_select_last))
            .on_action(cx.listener(Self::on_copy_text))
            .on_action(cx.listener(Self::on_reveal_selected))
            .on_action(cx.listener(Self::on_remove_selected))
            .on_action(cx.listener(Self::on_cancel_selected))
            .on_action(cx.listener(Self::on_text_larger))
            .on_action(cx.listener(Self::on_text_smaller))
            .on_action(cx.listener(Self::on_text_reset))
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
                    .child(Self::action_button(
                        Action::Text,
                        &self.text_focus.clone(),
                        window,
                        cx,
                    ))
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
            .when(!empty, |this| this.child(self.render_list(cx)))
            .when(has_done, |this| {
                this.child(
                    div().flex().child(
                        small_button(
                            ("clear", 0),
                            "Clear finished (⌘K)",
                            cx.listener(|this, _: &ClickEvent, _, cx| this.clear_done(cx)),
                        )
                        .debug_selector(|| "clear-done".to_string())
                        .track_focus(&self.clear_focus)
                        .focus(|style| style.border_color(rgb(ACCENT))),
                    ),
                )
            })
    }
}

/// A labelled button for the pointer. Keyboard focus is added by the caller
/// where the button is a tab stop (Clear finished).
fn small_button(
    id: (&'static str, usize),
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || format!("{}-{}", id.0, id.1))
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
        window_min_size: Some(size(px(MIN_WINDOW.0), px(MIN_WINDOW.1))),
        titlebar: Some(TitlebarOptions {
            title: Some("PDFTextract".into()),
            ..TitlebarOptions::default()
        }),
        ..WindowOptions::default()
    };
    match cx.open_window(options, |window, cx| {
        let (focus, scale) = {
            let shell = shell.read(cx);
            (shell.text_focus.clone(), shell.text_scale)
        };
        window.set_rem_size(px(scale.rem_px()));
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
        KeyBinding::new("cmd-=", TextLarger, Some("Shell")),
        KeyBinding::new("cmd-+", TextLarger, Some("Shell")),
        KeyBinding::new("cmd--", TextSmaller, Some("Shell")),
        KeyBinding::new("cmd-0", TextReset, Some("Shell")),
        KeyBinding::new("up", SelectPrev, Some("Jobs")),
        KeyBinding::new("down", SelectNext, Some("Jobs")),
        KeyBinding::new("home", SelectFirst, Some("Jobs")),
        KeyBinding::new("end", SelectLast, Some("Jobs")),
        KeyBinding::new("cmd-up", SelectFirst, Some("Jobs")),
        KeyBinding::new("cmd-down", SelectLast, Some("Jobs")),
        KeyBinding::new("cmd-c", CopyText, Some("Jobs")),
        KeyBinding::new("backspace", RemoveSelected, Some("Jobs")),
        KeyBinding::new("delete", RemoveSelected, Some("Jobs")),
        KeyBinding::new("escape", CancelSelected, Some("Jobs")),
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
                MenuItem::action("Copy Text", CopyText),
                MenuItem::action("Show in Finder", RevealSelected),
                MenuItem::action("Cancel Job", CancelSelected),
                MenuItem::action("Clear Finished", ClearDone),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Bigger Text", TextLarger),
                MenuItem::action("Smaller Text", TextSmaller),
                MenuItem::action("Actual Size", TextReset),
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
