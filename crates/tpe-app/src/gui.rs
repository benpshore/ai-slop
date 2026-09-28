//! GPUI front end (macOS only; the whole module is behind
//! `#[cfg(target_os = "macos")]` in `main.rs`).
//!
//! Three panes: corpus list (left), document view (centre: page text with
//! reading-order line numbers and citation markers, plus references), and the
//! Ask panel (right). The GUI holds no logic of its own beyond wiring; labels,
//! numbering and request handling live in the `tpe_app` library.
//!
//! # GPUI 0.2.2 items used (`file:line` in the crate source)
//!
//! - `Application::new().run(|cx| ..)`: `src/app.rs:132`, `src/app.rs:174`
//!   (`examples/hello_world.rs:90`).
//! - `App::open_window(WindowOptions, |window, cx| cx.new(..))`: `src/app.rs:943`;
//!   `WindowOptions` fields and `Default`: `src/platform.rs` (struct at the
//!   `pub struct WindowOptions` line, `impl Default` at `:1221`); `TitlebarOptions`
//!   (`#[derive(Default)]`): `src/platform.rs:1247`; `WindowBounds::Windowed`:
//!   `src/platform.rs:1188`; `Bounds::centered`: `src/geometry.rs:771`;
//!   `size`/`px`: `src/geometry.rs` (`px` at `:3598`).
//! - `App::activate`: `src/app.rs:979`; `App::quit`: `src/app.rs:749`;
//!   `App::bind_keys`: `src/app.rs:1677`; `App::on_action`: `src/app.rs:1696`;
//!   `App::focus_handle`: `src/app.rs:2029`; `App::background_executor`: `src/app.rs:1402`.
//! - `Render` trait: `src/element.rs:131`; `Context::listener`: `src/app/context.rs:252`;
//!   `Context::processor`: `src/app/context.rs:264`; `Context::notify`: `:229`;
//!   `Context::spawn` (async closure over `WeakEntity`, `AsyncApp`): `:237`;
//!   `Context: Deref<Target = App>`: `src/app/context.rs:26`;
//!   `WeakEntity::update`: `src/app/entity_map.rs:691`;
//!   `BackgroundExecutor::spawn`: `src/executor.rs:145`; `Task: Future`: `:100`;
//!   `Task::detach`: `:76`.
//! - `div()`: `src/elements/div.rs:1223`; `InteractiveElement::{id, track_focus,
//!   tab_stop, tab_index, key_context, hover, on_action, on_key_down}`:
//!   `src/elements/div.rs:607/616/628/637/658/670/854/881`;
//!   `StatefulInteractiveElement::{focus, overflow_y_scroll, on_click}`:
//!   `src/elements/div.rs:1020/1062/1117`; `ParentElement::{child, children}`:
//!   `src/element.rs:161/170`; `FluentBuilder::when`: `src/util.rs:23`.
//! - `Styled` methods: `src/styled.rs` (`flex` `:44`, `whitespace_normal` `:65`,
//!   `whitespace_nowrap` `:74`, `truncate` `:123`, `flex_col` `:137`, `flex_1` `:165`,
//!   `flex_shrink_0` `:221`, `items_start` `:249`, `items_center` `:263`,
//!   `justify_between` `:299`,
//!   `bg` `:372`, `text_color` `:396`, `font_weight` `:404`, `text_sm` `:442`,
//!   `font_family` `:616`); the `gpui_macros` generated helpers `size_full`,
//!   `w_full`, `h_full`, `w(px)`, `p_2`, `px_2`, `px_3`, `py_1`, `gap_1`, `gap_2`,
//!   `border_1`, `border_b_1`, `border_r_1`, `border_color`, `rounded_md`,
//!   `overflow_hidden`, `cursor_pointer` are each used by the shipped examples
//!   (`examples/hello_world.rs`, `examples/tab_stop.rs`, `examples/uniform_list.rs`,
//!   `examples/data_table.rs`).
//! - Text: `String`/`&'static str` are elements (`src/elements/text.rs:77/69`);
//!   `SharedString: From<&str>` (`src/shared_string.rs:116`); `FontWeight::{SEMIBOLD,
//!   BOLD}`: `src/text_system.rs:674/676`; `rgb`: `src/color.rs:14`. Wrapping:
//!   `WhiteSpace::Normal` is the default (`src/style.rs:321-329`, `:416`) and a
//!   text element wraps at the available width only under `Normal`
//!   (`src/elements/text.rs:347-352`); `truncate()` is `overflow_hidden` +
//!   `whitespace_nowrap` + `text_ellipsis` (`src/styled.rs:123-125`).
//! - Lists: `uniform_list(id, count, f)`: `src/elements/uniform_list.rs:22`
//!   (uniform row heights only, `:1-5`; the page text uses a scroll container).
//! - Actions and keys: `actions!`: `src/action.rs:24`; `KeyBinding::new(keys, action,
//!   context)`: `src/keymap/binding.rs:33`; keystroke syntax and the `cmd--` form:
//!   `src/platform/keystroke.rs:115-163`; macOS key names `enter`, `backspace`,
//!   `tab`, `up`/`down`/`left`/`right`: `src/platform/mac/events.rs:31-37,321-333`;
//!   `KeyDownEvent { keystroke, is_held }`: `src/interactive.rs:22`;
//!   `Keystroke { modifiers, key, key_char }`: `src/platform/keystroke.rs`.
//! - Focus: `FocusHandle::{tab_index, tab_stop, is_focused, contains_focused}`:
//!   `src/window.rs:312/323/345/351`; `Window::focus`: `src/window.rs:1386`;
//!   `Window::{focus_next, focus_prev}` exist (`:1413/1424`) but this GUI cycles
//!   panes explicitly; text scale via `Window::set_rem_size`: `src/window.rs:1830`.
//!
//! # Accessibility (verified against the 0.2.2 source)
//!
//! A search of `gpui-0.2.2/src` (including `platform/mac`), `Cargo.toml`,
//! `Cargo.lock`, `docs/` and `README.md` for `accessib`, `NSAccessibility`,
//! `accesskit` and `VoiceOver` finds nothing. GPUI 0.2.2 therefore exposes no
//! accessibility tree: `VoiceOver` sees the `NSWindow` but none of the elements,
//! roles, names or values. What the framework does provide, and this GUI uses:
//! keyboard focus and tab order (`FocusHandle`, `tab_index`), key bindings for
//! every command, visible text labels on every interactive element, and a global
//! text scale (`set_rem_size`). Tooltips (`div.rs:1161`) are visual only.
//! Screen-reader support would need a platform layer GPUI does not have yet.
//!
//! # Text input
//!
//! GPUI's IME-capable text input (`EntityInputHandler`, `src/input.rs:10`, shown
//! in `examples/input.rs`) is several hundred lines. The Ask box here is a
//! deliberately small `on_key_down` editor: printable `key_char`s append,
//! Backspace deletes, Enter sends. No cursor movement, selection or IME.

use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    App, Application, Bounds, ClickEvent, Context, Div, FocusHandle, FontWeight, KeyBinding,
    KeyDownEvent, Stateful, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, rgb, size, uniform_list,
};

use tpe_app::keys::{self, EnvKeyProvider, KeyProvider};
use tpe_app::ledger::{CorpusRow, DocumentDetail, LedgerReader};
use tpe_app::tpe_ai::{self, Provider};
use tpe_app::view::{
    self, AskRequest, AskTracker, CompletionVerdict, NumberedLine, Pane, TextScale,
};

actions!(
    workbench,
    [
        Quit,
        NextPane,
        PrevPane,
        TextLarger,
        TextSmaller,
        SendQuestion,
        ToggleProvider,
        SelectNext,
        SelectPrev,
        NextPage,
        PrevPage,
    ]
);

const BG: u32 = 0x0018_1a1f;
const PANEL: u32 = 0x0020_232a;
const BORDER: u32 = 0x003a_3f4a;
const ACCENT: u32 = 0x0058_a6ff;
const TEXT: u32 = 0x00e6_e6e6;
const MUTED: u32 = 0x009a_a0aa;
const BUTTON: u32 = 0x002f_3440;
const BUTTON_HOVER: u32 = 0x003d_4454;
const SELECTED: u32 = 0x0032_4a6d;
const MARKER: u32 = 0x00f2_c14e;

/// Width of the corpus and Ask panes and of the references column.
const SIDE_PANE_PX: f32 = 320.0;
/// Upper bound on the document text sent with a question.
const MAX_CONTEXT_CHARS: usize = 60_000;
/// Width of the line-number gutter.
const GUTTER_PX: f32 = 40.0;

/// Root view: owns the ledger reader, the loaded document and the Ask state.
pub struct Workbench {
    keys: Box<dyn KeyProvider>,
    reader: Option<LedgerReader>,
    corpus: Vec<CorpusRow>,
    selected: Option<usize>,
    detail: Option<DocumentDetail>,
    page_index: usize,
    lines: Vec<NumberedLine>,
    scale: TextScale,
    provider: Provider,
    question: String,
    answer: String,
    /// The request the shown `answer` belongs to; `None` for placeholders.
    answer_from: Option<AskRequest>,
    /// Issues request ids and remembers the request whose answer is awaited.
    ask: AskTracker,
    status: String,
    root_focus: FocusHandle,
    corpus_focus: FocusHandle,
    document_focus: FocusHandle,
    ask_focus: FocusHandle,
}

impl Workbench {
    fn new(ledger: &Path, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (reader, corpus, status) = match LedgerReader::open(ledger) {
            Ok(reader) => match reader.corpus() {
                Ok(corpus) => {
                    let status = format!("{} documents in {}", corpus.len(), ledger.display());
                    (Some(reader), corpus, status)
                }
                Err(error) => (None, Vec::new(), format!("Cannot read corpus: {error}")),
            },
            Err(error) => (
                None,
                Vec::new(),
                format!("Cannot open ledger {}: {error}", ledger.display()),
            ),
        };
        let corpus_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let document_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        let ask_focus = cx.focus_handle().tab_index(3).tab_stop(true);
        window.focus(&corpus_focus);
        Self {
            keys: Box::new(EnvKeyProvider),
            reader,
            corpus,
            selected: None,
            detail: None,
            page_index: 0,
            lines: Vec::new(),
            scale: TextScale::default(),
            provider: Provider::Anthropic,
            question: String::new(),
            answer: String::from("Ask a question about the selected document."),
            answer_from: None,
            ask: AskTracker::default(),
            status,
            root_focus: cx.focus_handle(),
            corpus_focus,
            document_focus,
            ask_focus,
        }
    }

    fn current_pane(&self, window: &Window, cx: &App) -> Pane {
        if self.document_focus.contains_focused(window, cx) {
            Pane::Document
        } else if self.ask_focus.contains_focused(window, cx) {
            Pane::Ask
        } else {
            Pane::Corpus
        }
    }

    fn focus_pane(&self, pane: Pane, window: &mut Window) {
        let handle = match pane {
            Pane::Corpus => &self.corpus_focus,
            Pane::Document => &self.document_focus,
            Pane::Ask => &self.ask_focus,
        };
        window.focus(handle);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.corpus.get(ix) else {
            return;
        };
        let run_id = row.run_id;
        let hash = row.hash.clone();
        self.selected = Some(ix);
        self.detail = None;
        self.lines.clear();
        self.page_index = 0;
        let outcome: Result<DocumentDetail, String> = match (run_id, self.reader.as_ref()) {
            (Some(run_id), Some(reader)) => reader
                .document(run_id)
                .map_err(|error| format!("Cannot load run {run_id}: {error}")),
            (None, _) => Err(format!(
                "{} has not been extracted yet",
                view::short_hash(&hash)
            )),
            (Some(_), None) => Err(String::from("The ledger is not open")),
        };
        match outcome {
            Ok(detail) => {
                self.status = format!(
                    "Loaded {}: {} pages, {} references, {} markers",
                    view::short_hash(&detail.hash),
                    detail.pages.len(),
                    detail.references.len(),
                    detail.citations.len()
                );
                self.detail = Some(detail);
                self.refresh_lines();
            }
            Err(message) => self.status = message,
        }
        cx.notify();
    }

    fn refresh_lines(&mut self) {
        let lines = match self.detail.as_ref() {
            Some(detail) => match detail.pages.get(self.page_index) {
                Some(page) => view::numbered_lines(&page.text, &detail.citations, page.page),
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        self.lines = lines;
    }

    fn step_page(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.detail.as_ref().map_or(0, |detail| detail.pages.len());
        if count == 0 {
            return;
        }
        let current = isize::try_from(self.page_index).unwrap_or(0);
        let last = isize::try_from(count - 1).unwrap_or(0);
        let next = (current + delta).clamp(0, last);
        self.page_index = usize::try_from(next).unwrap_or(0);
        self.refresh_lines();
        cx.notify();
    }

    fn step_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.corpus.is_empty() {
            return;
        }
        let last = isize::try_from(self.corpus.len() - 1).unwrap_or(0);
        let next = match self.selected {
            Some(ix) => (isize::try_from(ix).unwrap_or(0) + delta).clamp(0, last),
            None => 0,
        };
        self.select(usize::try_from(next).unwrap_or(0), cx);
    }

    fn set_scale(&mut self, scale: TextScale, window: &mut Window, cx: &mut Context<Self>) {
        self.scale = scale;
        window.set_rem_size(px(scale.rem_px()));
        cx.notify();
    }

    /// Content hash of the loaded document, the identity a request is tagged with.
    fn document_hash(&self) -> Option<String> {
        self.detail.as_ref().map(|detail| detail.hash.clone())
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let provider = self.provider;
        let document = self.document_hash();
        if self.ask.is_busy_for(provider, document.as_deref()) {
            return;
        }
        let question = self.question.trim().to_owned();
        if question.is_empty() {
            self.answer = String::from("Type a question first.");
            self.answer_from = None;
            cx.notify();
            return;
        }
        let Some(key) = self.keys.api_key(provider.credential_service()) else {
            self.answer = keys::missing_key_message(provider);
            self.answer_from = None;
            cx.notify();
            return;
        };
        let context = match self.detail.as_ref() {
            Some(detail) => view::document_context(detail, MAX_CONTEXT_CHARS),
            None => String::from("No document is selected. Say so, then answer briefly."),
        };
        let user = format!("{context}\n\nQuestion: {question}");
        let system = view::SYSTEM_PROMPT.to_owned();
        // Any earlier request still in flight is superseded by this id; its
        // answer is dropped in `finish_ask`.
        let request = self.ask.issue(provider, document.as_deref());
        self.answer = format!("Asking {} ...", request.label());
        self.answer_from = None;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            tpe_ai::ask(provider, &key, &system, &user).map_err(|error| format!("Error: {error}"))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_ask(&request, result, cx);
            });
        })
        .detach();
    }

    /// Applies a completed request only when it is still the latest one and
    /// the workbench shows the document and provider it was issued for
    /// (`view::completion_verdict`); a late answer for something the user has
    /// moved away from is dropped instead of overwriting the panel.
    fn finish_ask(
        &mut self,
        request: &AskRequest,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let document = self.document_hash();
        match self
            .ask
            .complete(request, self.provider, document.as_deref())
        {
            CompletionVerdict::Apply => {
                self.answer = match result {
                    Ok(text) | Err(text) => text,
                };
                self.answer_from = Some(request.clone());
            }
            CompletionVerdict::Stale => {
                let label = request.label();
                self.answer = format!(
                    "The answer from {label} arrived after you switched document or model \
                     and was discarded. Ask again."
                );
                self.answer_from = None;
                self.status = format!("Discarded a late answer from {label}");
            }
            CompletionVerdict::Superseded => return,
        }
        cx.notify();
    }

    // Action handlers (bound in `run`).

    fn on_next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.current_pane(window, cx).next();
        self.focus_pane(next, window);
        cx.notify();
    }

    fn on_prev_pane(&mut self, _: &PrevPane, window: &mut Window, cx: &mut Context<Self>) {
        let prev = self.current_pane(window, cx).prev();
        self.focus_pane(prev, window);
        cx.notify();
    }

    fn on_text_larger(&mut self, _: &TextLarger, window: &mut Window, cx: &mut Context<Self>) {
        self.set_scale(self.scale.larger(), window, cx);
    }

    fn on_text_smaller(&mut self, _: &TextSmaller, window: &mut Window, cx: &mut Context<Self>) {
        self.set_scale(self.scale.smaller(), window, cx);
    }

    fn on_toggle_provider(
        &mut self,
        _: &ToggleProvider,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.provider = self.provider.toggle();
        cx.notify();
    }

    fn on_select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(1, cx);
    }

    fn on_select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(-1, cx);
    }

    fn on_next_page(&mut self, _: &NextPage, _window: &mut Window, cx: &mut Context<Self>) {
        self.step_page(1, cx);
    }

    fn on_prev_page(&mut self, _: &PrevPage, _window: &mut Window, cx: &mut Context<Self>) {
        self.step_page(-1, cx);
    }

    fn on_send(&mut self, _: &SendQuestion, _window: &mut Window, cx: &mut Context<Self>) {
        self.send(cx);
    }

    /// Minimal editor for the question box (see the module doc).
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        match keystroke.key.as_str() {
            "backspace" => {
                self.question.pop();
            }
            "enter" | "tab" | "escape" | "up" | "down" | "left" | "right" => return,
            _ => {
                let Some(text) = keystroke.key_char.as_deref() else {
                    return;
                };
                if text.chars().any(char::is_control) {
                    return;
                }
                self.question.push_str(text);
            }
        }
        cx.notify();
    }

    // Rendering.

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let pane = self.current_pane(window, cx);
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child("Text Processing Engine"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .child(format!("Pane: {}", pane.label())),
                    )
                    .child(button(
                        "provider",
                        10,
                        format!("Model: {}", self.provider.label()),
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.provider = this.provider.toggle();
                            cx.notify();
                        }),
                    ))
                    .child(button(
                        "text-smaller",
                        11,
                        String::from("A-"),
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.set_scale(this.scale.smaller(), window, cx);
                        }),
                    ))
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .child(self.scale.percent_label()),
                    )
                    .child(button(
                        "text-larger",
                        12,
                        String::from("A+"),
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.set_scale(this.scale.larger(), window, cx);
                        }),
                    )),
            )
    }

    fn render_corpus(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let focused = self.corpus_focus.contains_focused(window, cx);
        let selected = self.selected;
        let count = self.corpus.len();
        pane_frame("corpus", &self.corpus_focus, "Corpus", focused)
            .w(px(SIDE_PANE_PX))
            .flex_shrink_0()
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_prev))
            .child(pane_title(format!("Corpus: {count} documents")))
            .child(
                div().flex_1().overflow_hidden().child(
                    uniform_list(
                        "corpus-list",
                        count,
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            range
                                .map(|ix| {
                                    let label = this
                                        .corpus
                                        .get(ix)
                                        .map_or_else(String::new, view::corpus_label);
                                    div()
                                        .id(ix)
                                        .px_2()
                                        .py_1()
                                        .cursor_pointer()
                                        .truncate()
                                        .when(selected == Some(ix), |row| row.bg(rgb(SELECTED)))
                                        .hover(|style| style.bg(rgb(BUTTON_HOVER)))
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _window, cx| {
                                                this.select(ix, cx);
                                            },
                                        ))
                                        .child(label)
                                })
                                .collect()
                        }),
                    )
                    .h_full(),
                ),
            )
    }

    fn render_document(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let focused = self.document_focus.contains_focused(window, cx);
        let frame = pane_frame("document", &self.document_focus, "Document", focused)
            .flex_1()
            .on_action(cx.listener(Self::on_next_page))
            .on_action(cx.listener(Self::on_prev_page));
        let Some(detail) = self.detail.as_ref() else {
            return frame.child(pane_title(String::from("Document"))).child(
                div()
                    .p_2()
                    .text_color(rgb(MUTED))
                    .child("Select a document in the corpus list (click it, or use Up/Down)."),
            );
        };
        let page_count = detail.pages.len();
        let page_label = detail.pages.get(self.page_index).map_or_else(
            || String::from("no pages"),
            |page| format!("Page {} of {page_count}", page.page),
        );
        let title = detail
            .title
            .clone()
            .unwrap_or_else(|| view::short_hash(&detail.hash).to_owned());
        let doi = detail
            .doi
            .as_deref()
            .map_or_else(String::new, |doi| format!("doi:{doi}  ·  "));
        let meta = format!(
            "{doi}{page_label}  ·  {} references  ·  {} markers  ·  {}",
            detail.references.len(),
            detail.citations.len(),
            detail.status
        );
        frame
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(button(
                                "prev-page",
                                20,
                                String::from("< Prev page"),
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.step_page(-1, cx);
                                }),
                            ))
                            .child(button(
                                "next-page",
                                21,
                                String::from("Next page >"),
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.step_page(1, cx);
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(meta),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_page_lines())
                    .child(render_references(detail, cx)),
            )
    }

    /// The page text as a vertically scrolling column of wrapped lines. This
    /// is a plain scroll container rather than a `uniform_list`: that element
    /// measures one item and gives every row the same height
    /// (`src/elements/uniform_list.rs:1-5`), which wrapped rows do not have.
    /// A page is at most a few hundred lines, so laying them all out is cheap.
    fn render_page_lines(&self) -> Stateful<Div> {
        div()
            .id("page-lines")
            .flex_1()
            .overflow_hidden()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(BORDER))
            .font_family("Menlo")
            .children(
                self.lines
                    .iter()
                    .enumerate()
                    .map(|(ix, line)| render_line(ix, line)),
            )
    }

    fn render_ask(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let focused = self.ask_focus.contains_focused(window, cx);
        let input_focused = self.ask_focus.is_focused(window);
        let empty = self.question.is_empty();
        let shown = if empty && !input_focused {
            String::from("Type a question and press Enter")
        } else if input_focused {
            format!("{}|", self.question)
        } else {
            self.question.clone()
        };
        let hint = format!(
            "Asks {} about the selected document. Tab / Shift-Tab: panes. Enter: send. \
             Cmd-P: switch model. Cmd-= / Cmd--: text size. Cmd-Q: quit.",
            self.provider.label()
        );
        pane_frame("ask", &self.ask_focus, "Ask", focused)
            .w(px(SIDE_PANE_PX))
            .flex_shrink_0()
            .on_action(cx.listener(Self::on_send))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(pane_title(format!("Ask {}", self.provider.label())))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_color(rgb(MUTED))
                    .whitespace_normal()
                    .child(hint),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .id("question")
                            .flex_1()
                            .p_2()
                            .rounded_md()
                            .bg(rgb(BG))
                            .border_1()
                            .border_color(rgb(if input_focused { ACCENT } else { BORDER }))
                            .text_color(rgb(if empty { MUTED } else { TEXT }))
                            .whitespace_normal()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, _cx| {
                                window.focus(&this.ask_focus);
                            }))
                            .child(shown),
                    )
                    .child(button(
                        "send",
                        30,
                        String::from(if self.ask.inflight().is_some() {
                            "Sending..."
                        } else {
                            "Send"
                        }),
                        cx.listener(|this, _: &ClickEvent, _window, cx| this.send(cx)),
                    )),
            )
            .child(pane_title(view::answer_title(self.answer_from.as_ref())))
            .child(
                div()
                    .id("answer")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .whitespace_normal()
                    .child(self.answer.clone()),
            )
    }

    fn render_status(&self) -> Div {
        div()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .text_color(rgb(MUTED))
            .truncate()
            .child(self.status.clone())
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("workbench")
            .key_context("Workbench")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(Self::on_next_pane))
            .on_action(cx.listener(Self::on_prev_pane))
            .on_action(cx.listener(Self::on_text_larger))
            .on_action(cx.listener(Self::on_text_smaller))
            .on_action(cx.listener(Self::on_toggle_provider))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(self.render_header(window, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(self.render_corpus(window, cx))
                    .child(self.render_document(window, cx))
                    .child(self.render_ask(window, cx)),
            )
            .child(self.render_status())
    }
}

/// A labelled, focusable button. The label is the accessible name for keyboard
/// users; GPUI has no accessibility tree to expose it to (see module doc).
fn button(
    id: &'static str,
    tab_index: isize,
    label: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .tab_index(tab_index)
        .px_2()
        .py_1()
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

/// Pane container: tracks its focus handle, carries the key context used by
/// the pane-specific bindings and highlights its border while focused.
fn pane_frame(
    id: &'static str,
    focus: &FocusHandle,
    key_context: &'static str,
    focused: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(focus)
        .key_context(key_context)
        .flex()
        .flex_col()
        .h_full()
        .overflow_hidden()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(if focused { ACCENT } else { BORDER }))
}

fn pane_title(text: String) -> Div {
    div()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(rgb(BORDER))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text)
}

/// One page-text row: gutter with the reading-order line number, the text,
/// and any citation markers whose offset falls on this line.
///
/// The text cell soft-wraps so that nothing extracted is hidden: GPUI text
/// wraps whenever its `white_space` is `Normal` (the default,
/// `src/style.rs:321-329` and `:416`) and the cell has a definite width
/// (`src/elements/text.rs:347-352`); `truncate()` would instead set
/// `nowrap` plus an ellipsis (`src/styled.rs:123-125`). The cell keeps
/// `overflow_hidden` so its flex minimum width is zero and it wraps at the
/// pane width rather than growing to the longest line. The gutter and marker
/// cells stay `nowrap` and the row uses `items_start` (`src/styled.rs:249`)
/// so the line number sits on the first wrapped line. An empty line still
/// takes one line height (`src/text_system/line_layout.rs:248-253`), so
/// paragraph breaks keep their spacing.
fn render_line(ix: usize, line: &NumberedLine) -> Stateful<Div> {
    let number = line.number.map_or_else(String::new, |n| n.to_string());
    let markers = line.markers.join("  ");
    div()
        .id(("line", ix))
        .flex()
        .items_start()
        .gap_2()
        .px_2()
        .child(
            div()
                .w(px(GUTTER_PX))
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(rgb(MUTED))
                .child(number),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_normal()
                .child(line.text.clone()),
        )
        .when(!markers.is_empty(), |row| {
            row.child(
                div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(rgb(MARKER))
                    .child(markers),
            )
        })
}

fn list_row(id: (&'static str, usize), label: String) -> Stateful<Div> {
    div().id(id).px_2().py_1().truncate().child(label)
}

/// References and citation markers column of the document view.
fn render_references(detail: &DocumentDetail, cx: &mut Context<Workbench>) -> Div {
    let ref_count = detail.references.len();
    let cite_count = detail.citations.len();
    div()
        .w(px(SIDE_PANE_PX))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(pane_title(format!("References ({ref_count})")))
        .child(
            div().flex_1().overflow_hidden().child(
                uniform_list(
                    "references",
                    ref_count,
                    cx.processor(|this, range: Range<usize>, _window, _cx| {
                        let Some(loaded) = this.detail.as_ref() else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|ix| {
                                loaded.references.get(ix).map(|entry| {
                                    list_row(("ref", ix), view::reference_label(entry))
                                })
                            })
                            .collect()
                    }),
                )
                .h_full(),
            ),
        )
        .child(pane_title(format!("Citation markers ({cite_count})")))
        .child(
            div().flex_1().overflow_hidden().child(
                uniform_list(
                    "citations",
                    cite_count,
                    cx.processor(|this, range: Range<usize>, _window, _cx| {
                        let Some(loaded) = this.detail.as_ref() else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|ix| {
                                loaded.citations.get(ix).map(|marker| {
                                    list_row(("cite", ix), view::citation_label(marker))
                                })
                            })
                            .collect()
                    }),
                )
                .h_full(),
            ),
        )
}

/// Starts the application, binds the keys and opens the window on `ledger`.
pub fn run(ledger: PathBuf) {
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-p", ToggleProvider, Some("Workbench")),
            KeyBinding::new("cmd-=", TextLarger, Some("Workbench")),
            KeyBinding::new("cmd--", TextSmaller, Some("Workbench")),
            KeyBinding::new("tab", NextPane, Some("Workbench")),
            KeyBinding::new("shift-tab", PrevPane, Some("Workbench")),
            KeyBinding::new("up", SelectPrev, Some("Corpus")),
            KeyBinding::new("down", SelectNext, Some("Corpus")),
            KeyBinding::new("left", PrevPage, Some("Document")),
            KeyBinding::new("right", NextPage, Some("Document")),
            KeyBinding::new("enter", SendQuestion, Some("Ask")),
        ]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Text Processing Engine".into()),
                ..TitlebarOptions::default()
            }),
            ..WindowOptions::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            cx.new(|cx| Workbench::new(&ledger, window, cx))
        });
        match opened {
            Ok(_) => cx.activate(true),
            Err(error) => {
                eprintln!("cannot open window: {error}");
                cx.quit();
            }
        }
    });
}
