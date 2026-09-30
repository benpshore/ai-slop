//! Headless tests of the window, run under GPUI's test platform: real view,
//! real key bindings, real engine on the synthetic paper; only the file
//! chooser, Finder and `quit` are replaced (`Host`), because the test
//! platform cannot answer them. They run on the macOS App workflow. GPUI has
//! no public way to build a file drop, so drops are covered through
//! `Shell::enqueue` (what the drop listener calls) and the Finder mailbox.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyWindowHandle, Entity, Keystroke, Pixels, TestAppContext, VisualTestContext, px, size,
};

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

/// Copies of the fixture paper named `p0.pdf`, `p1.pdf`, …
fn copies(fixture: &Fixture, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|n| {
            let path = fixture.dir.path().join(format!("p{n}.pdf"));
            std::fs::copy(FIXTURE, &path).unwrap();
            path
        })
        .collect()
}

fn ids(shell: &Entity<Shell>, cx: &TestAppContext) -> Vec<usize> {
    shell.read_with(cx, |shell, _| {
        shell.jobs.rows().iter().map(|row| row.id).collect()
    })
}

fn selected(shell: &Entity<Shell>, cx: &TestAppContext) -> Option<usize> {
    shell.read_with(cx, |shell, _| shell.selected)
}

/// Rows added without starting the engine, so they stay queued.
fn queue_only(shell: &Entity<Shell>, cx: &mut TestAppContext, paths: Vec<PathBuf>) {
    shell.update(cx, |shell, cx| {
        shell.jobs.enqueue(paths, Action::Text);
        cx.notify();
    });
}

/// Draw the window once and return the bounds of a `debug_selector`.
fn bounds_of(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    shell: &Entity<Shell>,
    selector: &'static str,
) -> Option<gpui::Bounds<Pixels>> {
    let mut visual = VisualTestContext::from_window(window, cx);
    // Twice: a scroll requested by a key is applied while the list lays out,
    // so the rows for the new position are built by the second frame.
    for _ in 0..2 {
        visual.draw(
            gpui::point(px(0.0), px(0.0)),
            size(px(640.0), px(480.0)),
            |_, _| shell.clone(),
        );
    }
    visual.debug_bounds(selector)
}

/// Three finished rows and the window with focus on the list (Tab from Get
/// text: Get bibliography, then the list), first row selected.
fn three_finished_rows(cx: &mut TestAppContext) -> (Fixture, Entity<Shell>, AnyWindowHandle) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    fixture.mailbox.send((Action::Text, copies(&fixture, 3)));
    cx.run_until_parked();
    assert!(
        row_phase(&shell, cx)
            .iter()
            .all(|phase| matches!(phase, Phase::Finished(_)))
    );
    keys(cx, window, "tab tab");
    (fixture, shell, window)
}

#[gpui::test]
fn tabbing_into_the_list_selects_its_first_row(cx: &mut TestAppContext) {
    let (_fixture, shell, window) = three_finished_rows(cx);
    let ids = ids(&shell, cx);
    assert_eq!(selected(&shell, cx), Some(ids[0]));
    let focused = cx
        .update_window(window, |_, window, cx| {
            shell.read(cx).list_focus.is_focused(window)
        })
        .unwrap();
    assert!(focused);
}

#[gpui::test]
fn arrow_keys_move_the_selection_and_stop_at_the_ends(cx: &mut TestAppContext) {
    let (_fixture, shell, window) = three_finished_rows(cx);
    let ids = ids(&shell, cx);
    keys(cx, window, "down");
    assert_eq!(selected(&shell, cx), Some(ids[1]));
    keys(cx, window, "down down down");
    assert_eq!(selected(&shell, cx), Some(ids[2]), "stops at the last row");
    keys(cx, window, "up");
    assert_eq!(selected(&shell, cx), Some(ids[1]));
    keys(cx, window, "home");
    assert_eq!(selected(&shell, cx), Some(ids[0]));
    keys(cx, window, "up");
    assert_eq!(selected(&shell, cx), Some(ids[0]), "stops at the first row");
    keys(cx, window, "end");
    assert_eq!(selected(&shell, cx), Some(ids[2]));
    keys(cx, window, "cmd-up");
    assert_eq!(selected(&shell, cx), Some(ids[0]));
    keys(cx, window, "cmd-down");
    assert_eq!(selected(&shell, cx), Some(ids[2]));
}

#[gpui::test]
fn enter_shows_the_selected_rows_result_in_finder(cx: &mut TestAppContext) {
    let (fixture, _shell, window) = three_finished_rows(cx);
    keys(cx, window, "down enter");
    assert_eq!(
        *fixture.log.reveals.borrow(),
        [fixture.dir.path().join("p1.txt")]
    );
    keys(cx, window, "space");
    assert_eq!(
        fixture.log.reveals.borrow().len(),
        2,
        "space does the same as enter"
    );
}

#[gpui::test]
fn cmd_c_copies_the_selected_rows_text(cx: &mut TestAppContext) {
    let (_fixture, _shell, window) = three_finished_rows(cx);
    keys(cx, window, "cmd-c");
    let text = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("text on the clipboard");
    assert!(text.contains("Faithful Extraction of Citations from Academic PDFs"));
}

#[gpui::test]
fn clear_finished_is_the_stop_after_the_list(cx: &mut TestAppContext) {
    let (_fixture, shell, window) = three_finished_rows(cx);
    keys(cx, window, "tab enter");
    assert!(row_phase(&shell, cx).is_empty());
    assert_eq!(
        selected(&shell, cx),
        None,
        "nothing is selected once nothing is listed"
    );
}

#[gpui::test]
fn delete_removes_a_queued_row_and_leaves_finished_ones(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    queue_only(&shell, cx, copies(&fixture, 3));
    let ids = ids(&shell, cx);
    keys(cx, window, "tab tab down");
    assert_eq!(selected(&shell, cx), Some(ids[1]));
    keys(cx, window, "backspace");
    assert_eq!(super::tests::ids(&shell, cx), [ids[0], ids[2]]);
    assert_eq!(
        selected(&shell, cx),
        Some(ids[2]),
        "selection lands on the next row"
    );
    keys(cx, window, "delete");
    assert_eq!(super::tests::ids(&shell, cx), [ids[0]]);
    assert_eq!(
        selected(&shell, cx),
        Some(ids[0]),
        "or the last one when there is no next"
    );

    // A finished row is not removed by a stray key.
    fixture
        .mailbox
        .send((Action::Text, vec![fixture.pdf.clone()]));
    shell.update(cx, |shell, cx| {
        shell.jobs.clear_done();
        cx.notify();
    });
    cx.run_until_parked();
    keys(cx, window, "end");
    let before = row_phase(&shell, cx);
    keys(cx, window, "backspace");
    assert_eq!(row_phase(&shell, cx), before);
}

#[gpui::test]
fn keys_reach_rows_that_are_off_screen(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    queue_only(&shell, cx, copies(&fixture, 200));
    let ids = ids(&shell, cx);
    // The list measures row 0 once, so "not built" is checked on a middle row.
    let middle: &'static str = Box::leak(format!("job-{}", ids[100]).into_boxed_str());
    let last: &'static str = Box::leak(format!("job-{}", ids[199]).into_boxed_str());
    assert!(
        bounds_of(cx, window, &shell, "job-1").is_some(),
        "the first row is drawn"
    );
    assert!(
        bounds_of(cx, window, &shell, last).is_none(),
        "the last row is not built yet"
    );

    keys(cx, window, "tab tab end");
    assert_eq!(selected(&shell, cx), Some(ids[199]));
    assert!(
        bounds_of(cx, window, &shell, last).is_some(),
        "End scrolled the last row into view"
    );
    assert!(
        bounds_of(cx, window, &shell, middle).is_none(),
        "rows in between are not built"
    );

    let offset = |cx: &TestAppContext| {
        shell.read_with(cx, |shell, _| {
            f32::from(shell.scroll.0.borrow().base_handle.offset().y)
        })
    };
    assert!(offset(cx) < -1000.0, "End scrolled the list down");

    keys(cx, window, "home");
    assert_eq!(selected(&shell, cx), Some(ids[0]));
    let _ = bounds_of(cx, window, &shell, "job-1");
    assert!(offset(cx).abs() < 0.5, "Home scrolled back to the top");
    assert!(bounds_of(cx, window, &shell, "job-1").is_some());
}

#[gpui::test]
fn clicking_a_row_selects_it_and_focuses_the_list(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    queue_only(&shell, cx, copies(&fixture, 3));
    let ids = ids(&shell, cx);
    let row = bounds_of(cx, window, &shell, "job-2").expect("row 2 is drawn");
    VisualTestContext::from_window(window, cx)
        .simulate_click(row.center(), gpui::Modifiers::none());
    assert_eq!(selected(&shell, cx), Some(ids[1]));
    let focused = cx
        .update_window(window, |_, window, cx| {
            shell.read(cx).list_focus.is_focused(window)
        })
        .unwrap();
    assert!(focused);
}

#[gpui::test]
fn rows_are_uniform_and_their_content_fits(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let junk = fixture.dir.path().join("junk.pdf");
    std::fs::write(&junk, b"not a pdf").unwrap();
    fixture
        .mailbox
        .send((Action::Text, vec![fixture.pdf.clone(), junk]));
    cx.run_until_parked();
    let row_1 = bounds_of(cx, window, &shell, "job-1").expect("finished row");
    let row_2 = bounds_of(cx, window, &shell, "job-2").expect("failed row");
    assert_eq!(
        row_1.size.height, row_2.size.height,
        "rows are the same height"
    );
    for (row, title, status) in [
        (row_1, "job-1-title", "job-1-status"),
        (row_2, "job-2-title", "job-2-status"),
    ] {
        for part in [title, status] {
            let inner = bounds_of(cx, window, &shell, part).expect("content is drawn");
            assert!(
                inner.top() >= row.top() && inner.bottom() <= row.bottom(),
                "{part} {inner:?} fits its row {row:?}"
            );
        }
    }
}

#[gpui::test]
fn drawing_cost_does_not_grow_with_the_number_of_rows(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    queue_only(&shell, cx, copies(&fixture, 5000));
    let mut visual = VisualTestContext::from_window(window, cx);
    let started = std::time::Instant::now();
    visual.draw(
        gpui::point(px(0.0), px(0.0)),
        size(px(640.0), px(480.0)),
        |_, _| shell.clone(),
    );
    let elapsed = started.elapsed();
    // Building every row took about 3 s at 5,000 rows in a debug build; only
    // the rows on screen are built now. The bound is generous on purpose.
    assert!(
        elapsed < std::time::Duration::from_millis(400),
        "one draw of 5,000 rows took {elapsed:?}"
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

/// One key, without stepping the executor: unlike `keys`, a job that is
/// running stays running (the test executor only advances when parked).
fn key_now(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.dispatch_keystroke(window, Keystroke::parse(key).unwrap());
}

/// The window with two queued files, the first running, and the list focused
/// with the running row selected, all without giving the engine a turn.
fn running_and_queued(cx: &mut TestAppContext) -> (Fixture, Entity<Shell>, AnyWindowHandle) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    shell.update(cx, |shell, cx| {
        shell.enqueue(copies(&fixture, 2), Action::Text, cx);
    });
    let phases = row_phase(&shell, cx);
    assert_eq!(phases, [Phase::Running, Phase::Queued]);
    key_now(cx, window, "tab");
    key_now(cx, window, "tab");
    assert_eq!(selected(&shell, cx), ids(&shell, cx).first().copied());
    (fixture, shell, window)
}

/// After the stop: the first row ended cancelled with nothing written for
/// it, and the queue moved on to the second.
fn assert_first_cancelled_second_finished(
    fixture: &Fixture,
    shell: &Entity<Shell>,
    cx: &TestAppContext,
) {
    let phases = row_phase(shell, cx);
    assert!(
        matches!(phases.as_slice(), [Phase::Cancelled, Phase::Finished(_)]),
        "{phases:?}"
    );
    assert!(
        !fixture.dir.path().join("p0.txt").exists(),
        "nothing for the cancelled job"
    );
    assert!(
        fixture.dir.path().join("p1.txt").exists(),
        "the next job ran"
    );
}

#[gpui::test]
fn escape_stops_the_running_job_and_the_queue_moves_on(cx: &mut TestAppContext) {
    let (fixture, shell, window) = running_and_queued(cx);
    key_now(cx, window, "escape");
    let (phase, cancelling) = shell.read_with(cx, |shell, _| {
        let row = &shell.jobs.rows()[0];
        (row.phase.clone(), row.cancelling)
    });
    assert_eq!(
        (phase, cancelling),
        (Phase::Running, true),
        "a stop is pending, shown at once"
    );
    cx.run_until_parked();
    assert_first_cancelled_second_finished(&fixture, &shell, cx);
}

#[gpui::test]
fn delete_on_a_running_row_stops_it_too(cx: &mut TestAppContext) {
    let (fixture, shell, window) = running_and_queued(cx);
    key_now(cx, window, "backspace");
    cx.run_until_parked();
    assert_first_cancelled_second_finished(&fixture, &shell, cx);
}

#[gpui::test]
fn the_running_row_offers_cancel_and_shows_a_pending_stop(cx: &mut TestAppContext) {
    // A simulated click steps the executor between mouse-down and mouse-up,
    // which would finish the job first; so the button's presence is checked
    // and its handler (`Shell::cancel`) is called directly.
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    shell.update(cx, |shell, cx| {
        shell.enqueue(copies(&fixture, 2), Action::Text, cx);
    });
    assert!(
        bounds_of(cx, window, &shell, "cancel-1").is_some(),
        "the running row has a Cancel button"
    );
    assert!(
        bounds_of(cx, window, &shell, "cancel-2").is_none(),
        "a queued row has Remove, not Cancel"
    );
    shell.update(cx, |shell, cx| shell.cancel(1, cx));
    let status = shell.read_with(cx, |shell, _| shell.jobs.rows()[0].status_line());
    assert_eq!(
        status, "text · Cancelling",
        "a stop is pending, shown at once"
    );
    cx.run_until_parked();
    assert_first_cancelled_second_finished(&fixture, &shell, cx);
}

#[gpui::test]
fn escape_leaves_queued_and_finished_rows_alone(cx: &mut TestAppContext) {
    let (_fixture, shell, window) = running_and_queued(cx);
    key_now(cx, window, "down");
    key_now(cx, window, "escape");
    assert_eq!(
        row_phase(&shell, cx),
        [Phase::Running, Phase::Queued],
        "a queued row is not touched"
    );
    cx.run_until_parked();
    let phases = row_phase(&shell, cx);
    assert!(
        phases
            .iter()
            .all(|phase| matches!(phase, Phase::Finished(_))),
        "{phases:?}"
    );
    keys(cx, window, "escape");
    assert_eq!(
        row_phase(&shell, cx),
        phases,
        "a finished row is not touched"
    );
}

#[gpui::test]
fn a_cancelled_row_can_be_shown_in_finder_and_cleared(cx: &mut TestAppContext) {
    let (fixture, shell, window) = running_and_queued(cx);
    key_now(cx, window, "escape");
    cx.run_until_parked();
    keys(cx, window, "enter");
    assert_eq!(
        *fixture.log.reveals.borrow(),
        [fixture.dir.path().join("p0.pdf")],
        "the source is shown"
    );
    keys(cx, window, "tab enter");
    assert!(
        row_phase(&shell, cx).is_empty(),
        "Clear finished removed the cancelled row too"
    );
}

/// Rows added without starting the engine: the first `done` are failed
/// (finished), the rest queued.
fn finished_then_queued(
    shell: &Entity<Shell>,
    cx: &mut TestAppContext,
    paths: Vec<PathBuf>,
    done: usize,
) -> Vec<usize> {
    queue_only(shell, cx, paths);
    let all = ids(shell, cx);
    shell.update(cx, |shell, _| {
        for &id in &all[..done] {
            shell.jobs.start(id);
            shell.jobs.finish(id, Err(jobs::RunError::Failed("failed".into())));
        }
    });
    all
}

#[gpui::test]
fn clearing_lands_on_the_first_survivor_after_the_selected_row(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    // [done, done (selected), running, queued]
    let rows = finished_then_queued(&shell, cx, copies(&fixture, 4), 2);
    shell.update(cx, |shell, _| {
        shell.jobs.start(rows[2]);
        shell.selected = Some(rows[1]);
    });
    keys(cx, window, "cmd-k");
    assert_eq!(ids(&shell, cx), [rows[2], rows[3]]);
    assert_eq!(
        selected(&shell, cx),
        Some(rows[2]),
        "the row that took the selected row's place, not the one after it"
    );
}

#[gpui::test]
fn clearing_keeps_a_surviving_selection_in_view(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    // 150 finished rows then 150 queued; the selected row is deep in the
    // queued ones, so after clearing it is at index 100 of 150 while the old
    // scroll offset points at the very end of the list.
    let rows = finished_then_queued(&shell, cx, copies(&fixture, 300), 150);
    keys(cx, window, "tab tab");
    shell.update(cx, |shell, _| shell.selected = Some(rows[250]));
    let _ = bounds_of(cx, window, &shell, "job-1");
    shell.update(cx, |shell, _| {
        shell.scroll.scroll_to_item(250, ScrollStrategy::Top);
    });
    let _ = bounds_of(cx, window, &shell, "job-1");

    keys(cx, window, "cmd-k");
    let _ = bounds_of(cx, window, &shell, "job-1");
    assert_eq!(selected(&shell, cx), Some(rows[250]));
    // Where the row is, from the scroll offset (a row's debug bounds from an
    // earlier draw would still answer, so they cannot say it moved).
    let at = ids(&shell, cx)
        .iter()
        .position(|&id| id == rows[250])
        .unwrap();
    let (top, height, row) = shell.read_with(cx, |shell, _| {
        let state = shell.scroll.0.borrow();
        (
            -f32::from(state.base_handle.offset().y),
            f32::from(state.last_item_size.unwrap().item.height),
            f32::from(gpui::px(16.0)) * ROW_HEIGHT_REMS,
        )
    });
    #[allow(clippy::cast_precision_loss)]
    let (from, to) = (at as f32 * row, (at + 1) as f32 * row);
    assert!(
        from >= top - 0.5 && to <= top + height + 0.5,
        "the surviving selected row (y {from}..{to}) is inside the viewport ({top}..{})",
        top + height
    );
}

#[gpui::test]
fn enter_does_not_reveal_a_queued_or_running_row(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let rows = finished_then_queued(&shell, cx, copies(&fixture, 2), 0);
    shell.update(cx, |shell, _| {
        shell.jobs.start(rows[0]);
        shell.selected = Some(rows[0]);
    });
    keys(cx, window, "tab tab enter");
    shell.update(cx, |shell, _| shell.selected = Some(rows[1]));
    keys(cx, window, "space");
    assert!(
        fixture.log.reveals.borrow().is_empty(),
        "no result yet: nothing to show, and never the input PDF"
    );
}
