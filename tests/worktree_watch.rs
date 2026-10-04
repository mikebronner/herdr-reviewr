//! Integration tests for the worktree watch that gates the poll, against real repositories
//! and the platform's real filesystem watcher.

mod common;

use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::Repo;
use herdr_reviewr::app::App;
use herdr_reviewr::model::Scope;
use herdr_reviewr::watch::WorktreeWatch;

/// Long enough that no test ever reaches the fallback.
const NO_FALLBACK: Duration = Duration::from_hours(1);
/// How long a test waits for a report that should arrive.
const DELIVERY: Duration = Duration::from_secs(10);
/// How long a test lets the watcher deliver before it asserts nothing arrived.
const SETTLE: Duration = Duration::from_millis(500);

/// A watch on `root`, running, with whatever the repository's setup left behind drained.
fn quiet_watch(root: &Path) -> WorktreeWatch {
    let mut watch = WorktreeWatch::start(root, NO_FALLBACK);
    let deadline = Instant::now() + DELIVERY;
    while !watch.is_watching() {
        assert!(Instant::now() < deadline, "the watcher starts");
        sleep(Duration::from_millis(20));
    }
    drain(&mut watch);
    watch
}

/// Let the watcher deliver, then clear whatever it reported.
fn drain(watch: &mut WorktreeWatch) {
    sleep(SETTLE);
    watch.poll_due(Instant::now());
}

/// Whether a poll finds a change before the delivery deadline.
fn reports_a_change(watch: &mut WorktreeWatch) -> bool {
    let deadline = Instant::now() + DELIVERY;
    while Instant::now() < deadline {
        if watch.poll_due(Instant::now()) {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    false
}

/// A committed repository, resolved the way `run` resolves it.
fn committed_repo() -> (Repo, std::path::PathBuf) {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let root = herdr_reviewr::git::toplevel(r.path()).expect("a repo");
    (r, root)
}

#[test]
fn a_quiet_worktree_reports_no_change_to_the_poll() {
    let (_r, root) = committed_repo();
    let mut watch = quiet_watch(&root);
    sleep(SETTLE);
    assert!(!watch.poll_due(Instant::now()), "nothing changed, so the poll builds nothing");
}

#[test]
fn an_edit_in_the_worktree_makes_the_next_poll_refresh() {
    let (r, root) = committed_repo();
    let mut watch = quiet_watch(&root);
    r.write("a.rs", "one\ntwo\n");
    assert!(reports_a_change(&mut watch), "the edit reaches the poll");
    drain(&mut watch);
    r.write("new.rs", "fresh\n");
    assert!(reports_a_change(&mut watch), "so does a new untracked file");
}

#[test]
fn a_commit_in_a_linked_worktree_makes_its_next_poll_refresh() {
    // A commit moves the branch ref in the common dir, which sits outside the linked worktree's
    // files, and leaves every worktree file as it was.
    let (r, _) = committed_repo();
    let place = tempfile::tempdir().unwrap();
    let linked = place.path().join("feature");
    r.git(&["worktree", "add", "-q", "-b", "feature", linked.to_str().unwrap()]);
    let root = herdr_reviewr::git::toplevel(&linked).expect("a linked worktree");
    let mut watch = quiet_watch(&root);
    r.git(&["-C", root.to_str().unwrap(), "commit", "-q", "--allow-empty", "-m", "empty"]);
    assert!(reports_a_change(&mut watch), "the commit reaches the poll");
}

#[test]
fn a_refresh_never_wakes_its_own_watch() {
    // A build in every git-heavy shape: an untracked file (the `ls-files` and attribute pass),
    // an edited tracked file, and `last-turn`, whose snapshot copies the index and writes
    // objects and a temporary index into the git dir. The snapshot's byte-copied index seed is
    // pinned only on macOS: there `fs::copy` made FSEvents report the real index. On Linux either
    // copy only reads the real index, which the watch ignores, so a revert passes there.
    let (r, root) = committed_repo();
    r.write("untracked.rs", "new\n");
    let baseline = herdr_reviewr::git::snapshot_worktree(&root).unwrap();
    herdr_reviewr::git::write_baseline_ref(&root, &baseline).unwrap();
    r.write("a.rs", "one\ntwo\n");
    let mut watch = quiet_watch(&root);
    let builds = |app: &mut App| {
        for scope in [Scope::Uncommitted, Scope::LastTurn] {
            app.scope = scope;
            let mut input = app.world_input();
            input.turn_baseline = Some(baseline.clone());
            herdr_reviewr::world::build(&input).unwrap();
        }
    };
    let mut app = App::new(root.clone(), Scope::Uncommitted, None);
    builds(&mut app);
    // A first round may refresh the index's stat data, which is a real index change.
    drain(&mut watch);
    builds(&mut app);
    sleep(SETTLE);
    assert!(!watch.poll_due(Instant::now()), "a settled refresh writes nothing a refresh reads");
}
