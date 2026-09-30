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

/// Marks an app whose `setup` has run.
struct SetupDone;

impl gpui::Global for SetupDone {}

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

    /// Bind the keys, build the view and open its window. `setup` runs once
    /// per app (it appends key bindings, so a second run would double them)
    /// and an earlier fixture's window is closed, so a test that opens
    /// several fixtures in one app measures one configuration, like the real
    /// app.
    fn open(&self, cx: &mut TestAppContext) -> (Entity<Shell>, AnyWindowHandle) {
        let ledger = self.dir.path().join("state").join("ledger.sqlite");
        let shell = cx.update(|cx| {
            if cx.try_global::<SetupDone>().is_none() {
                setup(cx);
                cx.set_global(SetupDone);
            }
            for stale in cx.windows() {
                let _ = stale.update(cx, |_, window, _| window.remove_window());
            }
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

/// Draw the window twice at `viewport` and return the bounds of a
/// `debug_selector` (bounds are clipped to what is visible in the viewport).
fn bounds_in(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    shell: &Entity<Shell>,
    selector: &'static str,
    viewport: gpui::Size<Pixels>,
) -> Option<gpui::Bounds<Pixels>> {
    let mut visual = VisualTestContext::from_window(window, cx);
    // Twice: a scroll requested by a key is applied while the list lays out,
    // so the rows for the new position are built by the second frame.
    for _ in 0..2 {
        visual.draw(gpui::point(px(0.0), px(0.0)), viewport, |_, _| {
            shell.clone()
        });
    }
    visual.debug_bounds(selector)
}

/// [`bounds_in`] at the default 640 by 480 viewport.
fn bounds_of(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    shell: &Entity<Shell>,
    selector: &'static str,
) -> Option<gpui::Bounds<Pixels>> {
    bounds_in(cx, window, shell, selector, size(px(640.0), px(480.0)))
}

/// A finished row, a failed one, and a failed one whose message is long
/// enough to wrap to the status's two lines: rows 1, 2 and 3.
fn rows_with_a_long_status(cx: &mut TestAppContext) -> (Fixture, Entity<Shell>, AnyWindowHandle) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let junk = fixture.dir.path().join("junk.pdf");
    std::fs::write(&junk, b"not a pdf").unwrap();
    fixture
        .mailbox
        .send((Action::Text, vec![fixture.pdf.clone(), junk]));
    cx.run_until_parked();
    queue_only(&shell, cx, copies(&fixture, 1));
    shell.update(cx, |shell, cx| {
        let message = "the message is long enough to need more than one line ".repeat(8);
        shell.jobs.finish(3, Err(jobs::RunError::Failed(message)));
        cx.notify();
    });
    (fixture, shell, window)
}

/// Every row of `ids` is the same height and its title and status lie inside
/// it, at `viewport`.
fn assert_rows_fit(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    shell: &Entity<Shell>,
    ids: &[usize],
    viewport: gpui::Size<Pixels>,
    when: &str,
) {
    let mut heights = Vec::new();
    for id in ids {
        let selector: &'static str = Box::leak(format!("job-{id}").into_boxed_str());
        let row = bounds_in(cx, window, shell, selector, viewport).expect("row drawn");
        heights.push(row.size.height);
        for part in ["title", "status"] {
            let name: &'static str = Box::leak(format!("job-{id}-{part}").into_boxed_str());
            let inner = bounds_in(cx, window, shell, name, viewport).expect("content drawn");
            assert!(
                inner.top() >= row.top() && inner.bottom() <= row.bottom(),
                "{name} {inner:?} fits its row {row:?} {when}"
            );
        }
    }
    assert!(
        heights.windows(2).all(|pair| pair[0] == pair[1]),
        "rows are the same height {when}: {heights:?}"
    );
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
    let (_fixture, shell, window) = rows_with_a_long_status(cx);
    let tall = size(px(1000.0), px(3000.0));
    assert_rows_fit(cx, window, &shell, &[1, 2, 3], tall, "at the base size");
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

/// Median of `samples` (sorts them).
fn median(samples: &mut [std::time::Duration]) -> std::time::Duration {
    samples.sort();
    let mid = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        // Even count: the average of the two middle observations.
        (samples[mid - 1] + samples[mid]) / 2
    } else {
        samples[mid]
    }
}

/// What a set of timings looks like: the middle *and* the tail. A median
/// alone hides jitter and the occasional slow interaction, which is exactly
/// what a user notices, so the table always shows the range and the 95th
/// percentile beside it.
#[derive(Debug, PartialEq, Eq)]
struct Stats {
    n: usize,
    min: std::time::Duration,
    median: std::time::Duration,
    /// Nearest-rank 95th percentile (the slowest one in twenty).
    p95: std::time::Duration,
    max: std::time::Duration,
}

impl Stats {
    fn of(samples: &mut [std::time::Duration]) -> Self {
        let median = median(samples); // sorts
        let n = samples.len();
        // Nearest rank: the smallest sample at or above 95% of them.
        let rank = (n * 95).div_ceil(100).max(1);
        Self {
            n,
            min: samples[0],
            median,
            p95: samples[rank - 1],
            max: samples[n - 1],
        }
    }
}

#[test]
fn the_median_of_an_even_count_averages_the_middle_pair() {
    let ms = std::time::Duration::from_millis;
    assert_eq!(median(&mut [ms(4), ms(1), ms(3), ms(2)]), ms(2) + ms(1) / 2);
    assert_eq!(median(&mut [ms(9), ms(1)]), ms(5));
    assert_eq!(median(&mut [ms(3), ms(1), ms(2)]), ms(2));
    assert_eq!(median(&mut [ms(7)]), ms(7));
}

#[test]
fn the_stats_keep_the_outliers_a_median_would_hide() {
    let ms = std::time::Duration::from_millis;
    // Four fast samples and one 100 ms stall: the median says 1 ms, the
    // range and the tail say the stall happened.
    let stats = Stats::of(&mut [ms(1), ms(100), ms(1), ms(1), ms(1)]);
    assert_eq!(
        stats,
        Stats {
            n: 5,
            min: ms(1),
            median: ms(1),
            p95: ms(100),
            max: ms(100)
        }
    );
    // 1..=100 ms: nearest-rank p95 is the 95th value.
    let mut hundred: Vec<_> = (1..=100).map(ms).collect();
    let stats = Stats::of(&mut hundred);
    assert_eq!((stats.p95, stats.max, stats.min), (ms(95), ms(100), ms(1)));
    assert_eq!(stats.median, ms(50) + ms(1) / 2);
    // A single sample is its own everything.
    let stats = Stats::of(&mut [ms(7)]);
    assert_eq!(
        (stats.min, stats.median, stats.p95, stats.max),
        (ms(7), ms(7), ms(7), ms(7))
    );
}

/// `f` timed `runs` times.
fn timed(runs: usize, mut f: impl FnMut()) -> Stats {
    let mut samples: Vec<_> = (0..runs)
        .map(|_| {
            let started = std::time::Instant::now();
            f();
            started.elapsed()
        })
        .collect();
    Stats::of(&mut samples)
}

/// One row of the published timings table.
fn row(what: &str, stats: &Stats) {
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    // How far the slowest sample is from the median: 1.0 is perfectly steady.
    let spread = ms(stats.max) / ms(stats.median).max(f64::EPSILON);
    eprintln!(
        "| {what} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {spread:.1}x |",
        stats.n,
        ms(stats.min),
        ms(stats.median),
        ms(stats.p95),
        ms(stats.max),
    );
}

/// One frame: layout, prepaint and paint of the window with the view.
fn frame(visual: &mut VisualTestContext, shell: &Entity<Shell>) {
    visual.draw(
        gpui::point(px(0.0), px(0.0)),
        size(px(640.0), px(480.0)),
        |_, _| shell.clone(),
    );
}

/// How long the interactions the app must answer at once take, on GPUI's
/// test platform: view state, layout and scene building on the CPU, with no
/// GPU and no display, so the frame is measured up to the point a renderer
/// would take over. The App workflow runs this on a macOS runner in release
/// mode and prints the table (`--release -- --ignored --nocapture`). It
/// measures; it asserts nothing.
#[gpui::test]
#[ignore = "a measurement: run with --release -- --ignored --nocapture"]
// One long table-printing script reads better than helpers per row.
#[allow(clippy::too_many_lines)]
fn interaction_timings(cx: &mut TestAppContext) {
    eprintln!("| interaction (ms) | n | min | median | p95 | max | max/median |");
    eprintln!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");

    // A window with no rows: the first frame, then a steady one.
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    let started = std::time::Instant::now();
    frame(&mut visual, &shell);
    row(
        "first frame, empty window (one cold draw)",
        &Stats::of(&mut [started.elapsed()]),
    );
    row(
        "frame, empty window",
        &timed(50, || frame(&mut visual, &shell)),
    );

    // A file arrives (a drop, a chooser, Finder) through `Shell::enqueue`,
    // which also starts the job: the row is there, running, drawn.
    let paths = copies(&fixture, 1);
    let mut arrivals = Vec::new();
    for _ in 0..30 {
        let started = std::time::Instant::now();
        shell.update(&mut visual, |shell, cx| {
            shell.enqueue(paths.clone(), Action::Text, cx);
        });
        frame(&mut visual, &shell);
        arrivals.push(started.elapsed());
        // Let the job finish, empty the list and draw the empty window, all
        // outside the timer, so the next arrival is into an already-drawn
        // empty view. The model is cleared directly: `Shell::clear_done` also
        // schedules a list scroll that an empty window (which draws no list)
        // would leave pending for the timed frame.
        visual.run_until_parked();
        shell.update(&mut visual, |shell, cx| {
            shell.jobs.clear_done();
            cx.notify();
        });
        frame(&mut visual, &shell);
        frame(&mut visual, &shell);
    }
    row("file arrives to its row drawn", &Stats::of(&mut arrivals));

    // The list at scale.
    for rows in [1usize, 100, 5000] {
        let scale = Fixture::new();
        let (shell, window) = scale.open(cx);
        queue_only(&shell, cx, copies(&scale, rows));
        let mut visual = VisualTestContext::from_window(window, cx);
        frame(&mut visual, &shell);
        let time = timed(20, || {
            shell.update(&mut visual, |_, cx| cx.notify());
            frame(&mut visual, &shell);
        });
        row(&format!("frame, {rows} rows"), &time);
    }

    // With 5,000 rows: a key press to the new selection drawn, a jump to the
    // end, and a progress event to its frame.
    let big = Fixture::new();
    let (shell, window) = big.open(cx);
    queue_only(&shell, cx, copies(&big, 5000));
    let mut visual = VisualTestContext::from_window(window, cx);
    frame(&mut visual, &shell);
    let list = shell.read_with(&visual, |shell, _| shell.list_focus.clone());
    visual.update(|window, _| window.focus(&list));
    // Start near the tail: `stepped` finds the selected row with a scan from
    // the front, so a press late in a big batch is the expensive case.
    let all = ids(&shell, &visual);
    shell.update(&mut visual, |shell, _| shell.selected = Some(all[4_849]));
    shell.update(&mut visual, |shell, _| {
        shell.scroll.scroll_to_item(4_849, ScrollStrategy::Top);
    });
    frame(&mut visual, &shell);
    frame(&mut visual, &shell);
    let mut presses = Vec::new();
    for _ in 0..100 {
        let started = std::time::Instant::now();
        visual
            .cx
            .dispatch_keystroke(window, Keystroke::parse("down").unwrap());
        frame(&mut visual, &shell);
        presses.push(started.elapsed());
    }
    row(
        "Down key to selection drawn, rows 4,850-4,950 of 5,000",
        &Stats::of(&mut presses),
    );
    let mut jumps = Vec::new();
    for key in ["end", "home"].repeat(10) {
        let started = std::time::Instant::now();
        visual
            .cx
            .dispatch_keystroke(window, Keystroke::parse(key).unwrap());
        frame(&mut visual, &shell);
        frame(&mut visual, &shell);
        jumps.push(started.elapsed());
    }
    row(
        "End / Home to the new rows drawn (two frames), 5,000 rows",
        &Stats::of(&mut jumps),
    );
    // The job being timed is the last row: every earlier one has finished
    // (a real batch reaches its late rows only after the early ones), so a
    // progress event pays for finding a row at the end of the list.
    let running = shell.update(&mut visual, |shell, _| {
        let ids: Vec<usize> = shell.jobs.rows().iter().map(|row| row.id).collect();
        let (tail, earlier) = ids.split_last().unwrap();
        for &id in earlier {
            shell.jobs.start(id);
            // As a successful text batch leaves them: a summary and a .txt
            // result, so the visible rows carry Copy and Show in Finder.
            shell.jobs.finish(
                id,
                Ok(jobs::Outcome {
                    outputs: vec![PathBuf::from(format!("/tmp/tpe-timing-{id}.txt"))],
                    summary: "text \u{b7} 2 pages, 3 references".into(),
                    warnings: Vec::new(),
                }),
            );
        }
        shell.jobs.start(*tail);
        *tail
    });
    // Show the running row: only the rows on screen are built, so with the
    // list left at the top the frames would draw unchanged rows.
    shell.update(&mut visual, |shell, _| {
        shell.scroll.scroll_to_item(4_999, ScrollStrategy::Bottom);
    });
    frame(&mut visual, &shell);
    frame(&mut visual, &shell);
    let mut page = 0u32;
    row(
        "progress event (model update, not the channel hop) to its frame, last of 5,000 rows",
        &timed(100, || {
            page += 1;
            shell.update(&mut visual, |shell, cx| {
                shell.jobs.progress(
                    running,
                    tpe::pipeline::Progress::Page {
                        page,
                        done: page,
                        total: 20_000,
                    },
                );
                cx.notify();
            });
            frame(&mut visual, &shell);
        }),
    );

    // The engine itself, on the two-page paper. Each sample gets its own
    // directory and a new ledger, so none is slowed by the outputs and
    // ledger rows of the ones before it (`publish` searches from the first
    // free name).
    tpe::pipeline::warm_up();
    for (what, action) in [
        (
            "Get text job on the 2-page paper (engine, new ledger, file)",
            Action::Text,
        ),
        (
            "Get bibliography job on the 2-page paper (engine, files)",
            Action::Bibliography,
        ),
    ] {
        let mut samples = Vec::new();
        for _ in 0..20 {
            let job = Fixture::new();
            let ledger = job.dir.path().join("timing-ledger.sqlite");
            let cancel = CancelToken::new();
            let started = std::time::Instant::now();
            jobs::run(action, &job.pdf, &ledger, &mut |_| {}, &cancel).unwrap();
            samples.push(started.elapsed());
        }
        row(what, &Stats::of(&mut samples));
    }
}

/// The window's root text size in pixels.
fn rem_px(cx: &mut TestAppContext, window: AnyWindowHandle) -> f32 {
    cx.update_window(window, |_, window, _| f32::from(window.rem_size()))
        .unwrap()
}

#[gpui::test]
fn cmd_plus_minus_and_zero_change_the_text_size(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (_shell, window) = fixture.open(cx);
    assert!(
        (rem_px(cx, window) - 16.0).abs() < 0.01,
        "starts at the base size"
    );
    keys(cx, window, "cmd-=");
    assert!((rem_px(cx, window) - 18.0).abs() < 0.01);
    keys(cx, window, "cmd-+");
    assert!(
        (rem_px(cx, window) - 20.0).abs() < 0.01,
        "cmd-+ is cmd-= with shift"
    );
    keys(cx, window, "cmd--");
    assert!((rem_px(cx, window) - 18.0).abs() < 0.01);
    keys(cx, window, "cmd-0");
    assert!(
        (rem_px(cx, window) - 16.0).abs() < 0.01,
        "cmd-0 is the actual size"
    );
    for _ in 0..20 {
        keys(cx, window, "cmd-=");
    }
    assert!((rem_px(cx, window) - 32.0).abs() < 0.01, "stops at 200%");
    for _ in 0..40 {
        keys(cx, window, "cmd--");
    }
    assert!((rem_px(cx, window) - 12.0).abs() < 0.01, "stops at 75%");
}

#[gpui::test]
fn the_text_size_survives_a_relaunch(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (_shell, window) = fixture.open(cx);
    keys(cx, window, "cmd-= cmd-=");
    let stored =
        std::fs::read_to_string(fixture.dir.path().join("state").join("text-scale")).unwrap();
    assert_eq!(stored.trim(), "1.25");

    // A second launch on the same settings directory opens at that size.
    let mut relaunched = cx.new_app();
    let (_shell, window) = fixture.open(&mut relaunched);
    assert!((rem_px(&mut relaunched, window) - 20.0).abs() < 0.01);
}

#[gpui::test]
fn a_damaged_settings_file_opens_at_the_base_size(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let settings = fixture.dir.path().join("state");
    std::fs::create_dir_all(&settings).unwrap();
    std::fs::write(settings.join("text-scale"), b"\xff\xfe huge please").unwrap();
    let (_shell, window) = fixture.open(cx);
    assert!((rem_px(cx, window) - 16.0).abs() < 0.01);
    keys(cx, window, "cmd-=");
    assert!(
        (rem_px(cx, window) - 18.0).abs() < 0.01,
        "and it can still be changed"
    );
}

#[gpui::test]
fn at_200_percent_the_controls_scale_and_rows_still_fit(cx: &mut TestAppContext) {
    let (_fixture, shell, window) = rows_with_a_long_status(cx);
    let tall = size(px(1000.0), px(3000.0));
    let button_1x = bounds_in(cx, window, &shell, "get-text", tall)
        .expect("button")
        .size
        .height;
    let row_1x = bounds_in(cx, window, &shell, "job-1", tall)
        .expect("row")
        .size
        .height;
    for _ in 0..8 {
        keys(cx, window, "cmd-=");
    }
    assert!((rem_px(cx, window) - 32.0).abs() < 0.01);
    let button_2x = bounds_in(cx, window, &shell, "get-text", tall)
        .expect("button")
        .size
        .height;
    let row_2x = bounds_in(cx, window, &shell, "job-1", tall)
        .expect("row")
        .size
        .height;
    assert!(
        f32::from(button_2x) >= 1.9 * f32::from(button_1x),
        "the big buttons scale: {button_1x:?} to {button_2x:?}"
    );
    assert!(
        (f32::from(row_2x) / f32::from(row_1x) - 2.0).abs() < 0.05,
        "rows scale: {row_1x:?} to {row_2x:?}"
    );
    assert_rows_fit(cx, window, &shell, &[1, 2, 3], tall, "at 200%");
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
            shell
                .jobs
                .finish(id, Err(jobs::RunError::Failed("failed".into())));
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

#[gpui::test]
fn at_the_smallest_window_a_whole_row_and_the_clear_button_fit(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let rows = finished_then_queued(&shell, cx, copies(&fixture, 3), 3);
    let (width, height) = MIN_WINDOW;
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_resize(size(px(width), px(height)));
    for _ in 0..2 {
        visual.draw(
            gpui::point(px(0.0), px(0.0)),
            size(px(width), px(height)),
            |_, _| shell.clone(),
        );
    }
    let first: &'static str = Box::leak(format!("job-{}", rows[0]).into_boxed_str());
    let row = visual.debug_bounds(first).unwrap();
    let clear = visual.debug_bounds("clear-done").unwrap();
    let rem = 16.0;
    assert!(
        (f32::from(row.size.height) - (rem * ROW_HEIGHT_REMS - 8.0)).abs() < 1.0,
        "the first row is whole (its cell less the 4 pt padding either side): {:?}",
        row.size
    );
    assert!(
        f32::from(clear.bottom()) <= height,
        "the Clear button is inside the window: {clear:?}"
    );
    assert!(
        f32::from(row.bottom()) <= f32::from(clear.top()),
        "the row ends above the Clear button: {row:?} {clear:?}"
    );
}

#[gpui::test]
fn tabbing_back_into_the_list_brings_the_selection_into_view(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (shell, window) = fixture.open(cx);
    let rows = finished_then_queued(&shell, cx, copies(&fixture, 200), 0);
    keys(cx, window, "tab tab");
    assert_eq!(
        selected(&shell, cx),
        Some(rows[0]),
        "the first row is selected"
    );
    let offset = |cx: &TestAppContext| {
        shell.read_with(cx, |shell, _| {
            f32::from(shell.scroll.0.borrow().base_handle.offset().y)
        })
    };
    let _ = bounds_of(cx, window, &shell, "job-1");
    shell.update(cx, |shell, _| {
        shell.scroll.scroll_to_item(150, ScrollStrategy::Top);
    });
    let _ = bounds_of(cx, window, &shell, "job-1");
    assert!(offset(cx) < -1000.0, "scrolled away from the selection");

    keys(cx, window, "shift-tab tab");
    let _ = bounds_of(cx, window, &shell, "job-1");
    assert_eq!(selected(&shell, cx), Some(rows[0]), "still the same row");
    assert!(
        offset(cx).abs() < 0.5,
        "the selection is back in view: offset {}",
        offset(cx)
    );
}
