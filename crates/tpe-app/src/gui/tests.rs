//! Headless tests of the window, run under GPUI's test platform: real view,
//! real key bindings, real engine on the synthetic paper; only the file
//! chooser, Finder and `quit` are replaced (`Host`), because the test
//! platform cannot answer them. They run on the macOS App workflow. GPUI has
//! no public way to build a file drop, so drops are covered through
//! `Shell::enqueue` (what the drop listener calls) and the Finder mailbox.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{AnyWindowHandle, Entity, TestAppContext, VisualTestContext};

use super::*;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/synthetic-paper.pdf"
);

/// What the fake platform saw and what it answers.
#[derive(Default)]
struct Log {
    /// Prompt titles of every file chooser opened.
    picks: RefCell<Vec<String>>,
    /// Paths the next chooser returns (`None`: the person cancels).
    answer: RefCell<Option<Vec<PathBuf>>>,
    reveals: RefCell<Vec<PathBuf>>,
    quits: RefCell<u32>,
}

fn fake_host(log: &Rc<Log>) -> Host {
    let (picks, reveals, quits) = (log.clone(), log.clone(), log.clone());
    Host {
        pick: Rc::new(move |_cx, options| {
            picks
                .picks
                .borrow_mut()
                .push(options.prompt.map(|p| p.to_string()).unwrap_or_default());
            let (sender, receiver) = oneshot::channel();
            let _ = sender.send(Ok(picks.answer.borrow_mut().take()));
            receiver
        }),
        reveal: Rc::new(move |_cx, path| reveals.reveals.borrow_mut().push(path.to_path_buf())),
        quit: Rc::new(move |_cx| *quits.quits.borrow_mut() += 1),
    }
}

/// A scratch directory holding a copy of the fixture paper, the fake
/// platform, and the mailbox the view listens on.
struct Fixture {
    dir: tempfile::TempDir,
    pdf: PathBuf,
    log: Rc<Log>,
    mailbox: Rc<Mailbox<Intake>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("paper.pdf");
        std::fs::copy(FIXTURE, &pdf).unwrap();
        Self {
            dir,
            pdf,
            log: Rc::new(Log::default()),
            mailbox: Rc::new(Mailbox::new()),
        }
    }

    /// Bind the keys, build the view and open its window.
    fn open(&self, cx: &mut TestAppContext) -> (Entity<Shell>, AnyWindowHandle) {
        let ledger = self.dir.path().join("state").join("ledger.sqlite");
        let shell = cx.update(|cx| {
            setup(cx);
            let shell = cx.new(|cx| Shell::new(&self.mailbox, ledger, fake_host(&self.log), cx));
            cx.set_global(ShellHandle(shell.clone()));
            open_main_window(cx);
            shell
        });
        cx.run_until_parked();
        let window = cx.windows()[0];
        (shell, window)
    }
}

fn keys(cx: &mut TestAppContext, window: AnyWindowHandle, keystrokes: &str) {
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_keystrokes(keystrokes);
}

fn row_phase(shell: &Entity<Shell>, cx: &TestAppContext) -> Vec<Phase> {
    shell.read_with(cx, |shell, _| {
        shell
            .jobs
            .rows()
            .iter()
            .map(|row| row.phase.clone())
            .collect()
    })
}

#[gpui::test]
fn the_window_opens_with_the_first_button_focused(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let focused = cx
        .update_window(window, |_, window, cx| {
            let shell = shell.read(cx);
            (
                shell.text_focus.is_focused(window),
                shell.biblio_focus.is_focused(window),
            )
        })
        .unwrap();
    assert_eq!(focused, (true, false));
}

#[gpui::test]
fn enter_and_tab_reach_both_actions(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (_shell, window) = fixture.open(cx);
    keys(cx, window, "enter");
    keys(cx, window, "tab enter");
    keys(cx, window, "space");
    keys(cx, window, "shift-tab enter");
    assert_eq!(
        *fixture.log.picks.borrow(),
        [
            "Get text",
            "Get bibliography",
            "Get bibliography",
            "Get text"
        ]
    );
}

#[gpui::test]
fn shortcuts_open_the_chooser_from_anywhere(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (_shell, window) = fixture.open(cx);
    keys(cx, window, "tab");
    keys(cx, window, "cmd-o");
    keys(cx, window, "cmd-b");
    assert_eq!(
        *fixture.log.picks.borrow(),
        ["Get text", "Get bibliography"]
    );
}

#[gpui::test]
fn a_cancelled_chooser_adds_nothing(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    keys(cx, window, "enter");
    cx.run_until_parked();
    assert_eq!(fixture.log.picks.borrow().len(), 1);
    assert!(row_phase(&shell, cx).is_empty());
}

#[gpui::test]
fn chosen_files_become_a_row_and_finish(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    *fixture.log.answer.borrow_mut() = Some(vec![fixture.pdf.clone()]);
    keys(cx, window, "enter");
    cx.run_until_parked();
    let phases = row_phase(&shell, cx);
    let [Phase::Finished(outcome)] = phases.as_slice() else {
        panic!("expected one finished row, got {phases:?}");
    };
    assert_eq!(outcome.summary, "2 pages, 3 references");
    assert_eq!(outcome.outputs, [fixture.dir.path().join("paper.txt")]);
    assert!(outcome.outputs[0].exists());
}

#[gpui::test]
fn a_file_is_a_running_row_before_the_engine_has_a_turn(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, _window) = fixture.open(cx);
    shell.update(cx, |shell, cx| {
        shell.enqueue(vec![fixture.pdf.clone()], Action::Text, cx);
    });
    // Nothing has yielded to the executor: the row is already on screen.
    let row = shell.read_with(cx, |shell, _| shell.jobs.rows().first().cloned());
    let row = row.expect("the row exists at once");
    assert_eq!(
        (row.phase.clone(), row.done, row.total),
        (Phase::Running, 0, None)
    );
    assert!(!fixture.dir.path().join("paper.txt").exists());

    cx.run_until_parked();
    assert!(matches!(
        row_phase(&shell, cx).as_slice(),
        [Phase::Finished(_)]
    ));
}

#[gpui::test]
fn files_sent_before_the_window_exists_are_queued_by_it(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    fixture
        .mailbox
        .send((Action::Bibliography, vec![fixture.pdf.clone()]));
    let (shell, _window) = fixture.open(cx);
    cx.run_until_parked();
    let actions = shell.read_with(cx, |shell, _| {
        shell
            .jobs
            .rows()
            .iter()
            .map(|row| row.action)
            .collect::<Vec<_>>()
    });
    assert_eq!(actions, [Action::Bibliography]);
    let phases = row_phase(&shell, cx);
    assert!(
        matches!(phases.as_slice(), [Phase::Finished(_)]),
        "{phases:?}"
    );
    assert!(fixture.dir.path().join("paper.references.json").exists());
}

#[gpui::test]
fn files_sent_while_open_are_queued_in_order(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, _window) = fixture.open(cx);
    let second = fixture.dir.path().join("second.pdf");
    std::fs::copy(FIXTURE, &second).unwrap();
    fixture
        .mailbox
        .send((Action::Text, vec![fixture.pdf.clone()]));
    fixture.mailbox.send((Action::Bibliography, vec![second]));
    cx.run_until_parked();
    let phases = row_phase(&shell, cx);
    assert!(
        matches!(phases.as_slice(), [Phase::Finished(_), Phase::Finished(_)]),
        "{phases:?}"
    );
}

#[gpui::test]
fn row_buttons_are_reachable_and_work_from_the_keyboard(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    fixture
        .mailbox
        .send((Action::Text, vec![fixture.pdf.clone()]));
    cx.run_until_parked();
    assert!(matches!(
        row_phase(&shell, cx).as_slice(),
        [Phase::Finished(_)]
    ));

    // Tab order: Get text, Get bibliography, Copy, Show in Finder, Clear.
    keys(cx, window, "tab tab enter");
    let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
    let text = clipboard.expect("Copy put the text on the clipboard");
    assert!(text.contains("Faithful Extraction of Citations from Academic PDFs"));

    keys(cx, window, "tab enter");
    assert_eq!(
        *fixture.log.reveals.borrow(),
        [fixture.dir.path().join("paper.txt")]
    );

    keys(cx, window, "tab enter");
    assert!(
        row_phase(&shell, cx).is_empty(),
        "Clear finished removed the row"
    );
}

#[gpui::test]
fn cmd_k_clears_finished_rows(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    fixture
        .mailbox
        .send((Action::Text, vec![fixture.pdf.clone()]));
    cx.run_until_parked();
    keys(cx, window, "cmd-k");
    assert!(row_phase(&shell, cx).is_empty());
}

#[gpui::test]
fn closing_the_window_keeps_running_work_and_quits_when_idle(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    // Enqueued directly, the job is running and the engine has not had a turn.
    shell.update(cx, |shell, cx| {
        shell.enqueue(vec![fixture.pdf.clone()], Action::Text, cx);
    });
    assert!(shell.read_with(cx, |shell, _| shell.jobs.has_active()));
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    assert_eq!(
        *fixture.log.quits.borrow(),
        0,
        "closing the window does not quit while work is active"
    );
    assert!(cx.windows().is_empty());

    cx.run_until_parked();
    assert!(
        matches!(row_phase(&shell, cx).as_slice(), [Phase::Finished(_)]),
        "the job finished"
    );
    assert!(fixture.dir.path().join("paper.txt").exists());
    assert_eq!(
        *fixture.log.quits.borrow(),
        1,
        "the app quits itself once idle"
    );
}

#[gpui::test]
fn closing_an_idle_window_quits(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (_shell, window) = fixture.open(cx);
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    assert_eq!(*fixture.log.quits.borrow(), 1);
}

#[gpui::test]
fn the_dock_icon_reopens_the_window_on_the_same_rows(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    shell.update(cx, |shell, cx| {
        shell.enqueue(vec![fixture.pdf.clone()], Action::Text, cx);
    });
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.update(|cx| {
        assert!(cx.windows().is_empty());
        open_main_window(cx);
    });
    assert_eq!(cx.windows().len(), 1);
    assert_eq!(shell.read_with(cx, |shell, _| shell.jobs.rows().len()), 1);
    cx.run_until_parked();
    assert!(matches!(
        row_phase(&shell, cx).as_slice(),
        [Phase::Finished(_)]
    ));
}

#[gpui::test]
fn a_bad_file_fails_its_row_and_the_queue_moves_on(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, _window) = fixture.open(cx);
    let junk = fixture.dir.path().join("junk.pdf");
    std::fs::write(&junk, b"not a pdf").unwrap();
    fixture
        .mailbox
        .send((Action::Text, vec![junk, fixture.pdf.clone()]));
    cx.run_until_parked();
    let phases = row_phase(&shell, cx);
    assert!(
        matches!(phases.as_slice(), [Phase::Failed(_), Phase::Finished(_)]),
        "{phases:?}"
    );
}
