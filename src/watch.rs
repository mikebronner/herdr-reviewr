//! Worktree change detection for the poll. A filesystem watcher records whether anything a
//! refresh reads may have changed, so a poll over a quiet worktree runs no git at all. Every
//! path that cannot watch counts as changed, which is the poll's old behavior.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecursiveMode, Watcher};

/// What the watcher thread shares with the poll.
#[derive(Debug)]
struct Shared {
    /// Every root is watched. Until then, and forever after a failed start, every poll
    /// counts as changed.
    watching: AtomicBool,
    /// The watcher reported an error, so it may have stopped seeing part of the tree. Sticky:
    /// from then on every poll counts as changed.
    lapsed: AtomicBool,
    /// Something a refresh reads changed since the poll last asked. Starts set, so the first
    /// poll catches whatever landed before the watcher started.
    changed: AtomicBool,
}

/// The poll's view of the worktree: whether a refresh is due.
#[derive(Debug)]
pub struct WorktreeWatch {
    shared: Arc<Shared>,
    /// A full refresh runs at least this often, even when the watcher reports nothing, as a
    /// net for events it never saw.
    fallback: Duration,
    last_full: Instant,
    /// Dropping it ends the thread that holds the watcher.
    _stop: Option<mpsc::Sender<()>>,
}

impl WorktreeWatch {
    /// Start watching `repo` and its git dirs on a thread of its own, so the walk a recursive
    /// watch makes on some platforms never delays a frame.
    pub fn start(repo: &Path, fallback: Duration) -> Self {
        let shared = Arc::new(Shared {
            watching: AtomicBool::new(false),
            lapsed: AtomicBool::new(false),
            changed: AtomicBool::new(true),
        });
        let (stop, stopped) = mpsc::channel::<()>();
        let (repo, thread_shared) = (repo.to_path_buf(), shared.clone());
        let spawned = std::thread::Builder::new().name("watch".into()).spawn(move || {
            match watch(&repo, thread_shared.clone()) {
                Ok(watcher) => {
                    thread_shared.watching.store(true, Ordering::SeqCst);
                    // Blocks until the owner drops its sender, keeping the watcher alive.
                    let _ = stopped.recv();
                    drop(watcher);
                }
                Err(e) => logln!("worktree watch unavailable, polling instead: {e}"),
            }
        });
        if let Err(e) = &spawned {
            logln!("worktree watch thread failed to start, polling instead: {e}");
        }
        Self { shared, fallback, last_full: Instant::now(), _stop: spawned.ok().map(|_| stop) }
    }

    /// Whether every root is watched, and the watcher has reported no error since.
    pub fn is_watching(&self) -> bool {
        self.shared.watching.load(Ordering::SeqCst) && !self.shared.lapsed.load(Ordering::SeqCst)
    }

    /// Whether this poll should refresh: something changed since the last poll, the watcher
    /// is not running, or the fallback interval has passed. Clears the change it reports.
    pub fn poll_due(&mut self, now: Instant) -> bool {
        let changed = self.shared.changed.swap(false, Ordering::SeqCst);
        let due = changed || !self.is_watching() || now >= self.last_full + self.fallback;
        if due {
            self.last_full = now;
        }
        due
    }
}

/// The watcher over every root, reporting into `shared`. Any root that fails to watch fails
/// the whole start, since a partial watch would miss commits until the fallback.
fn watch(repo: &Path, shared: Arc<Shared>) -> notify::Result<notify::RecommendedWatcher> {
    let repo = repo.canonicalize()?;
    let (git_dir, common) = crate::git::git_dirs(&repo)
        .ok_or_else(|| notify::Error::generic("could not resolve the git dirs"))?;
    let dirs = GitDirs { git: git_dir, common };
    let roots = watch_roots(&repo, &dirs);
    let mut watcher = notify::recommended_watcher(move |event| record(&shared, &event, &dirs))?;
    for root in roots {
        watcher.watch(&root, RecursiveMode::Recursive)?;
    }
    Ok(watcher)
}

/// The worktree's git dir and the repository's common dir. In a plain repository they are
/// the same `.git`; a linked worktree's git dir sits under the common dir.
#[derive(Debug)]
struct GitDirs {
    git: PathBuf,
    common: PathBuf,
}

/// The worktree, plus each git dir that lies outside every root already listed.
fn watch_roots(repo: &Path, dirs: &GitDirs) -> Vec<PathBuf> {
    let mut roots = vec![repo.to_path_buf()];
    for dir in [&dirs.common, &dirs.git] {
        if !roots.iter().any(|root| dir.starts_with(root)) {
            roots.push(dir.clone());
        }
    }
    roots
}

/// Record one watcher report. An error, such as Linux's inotify watch limit reached under a
/// new directory, means the watcher may no longer see the whole tree, so the poll stops
/// trusting it for good.
fn record(shared: &Shared, event: &notify::Result<Event>, dirs: &GitDirs) {
    match event {
        Err(e) => {
            logln!("worktree watch lapsed, polling instead: {e}");
            shared.lapsed.store(true, Ordering::SeqCst);
        }
        Ok(event) if event_affects_review(event, dirs) => {
            shared.changed.store(true, Ordering::SeqCst);
        }
        Ok(_) => {}
    }
}

/// Whether one watcher report may change what a refresh shows. A rescan report, or one with
/// no path, counts, because it cannot say what changed. A read never does.
fn event_affects_review(event: &Event, dirs: &GitDirs) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.need_rescan()
        || event.paths.is_empty()
        || event.paths.iter().any(|path| path_affects_review(path, dirs))
}

/// Whether a change at `path` may change what a refresh shows. A worktree file always may.
/// Inside the git dirs only what a refresh reads can: `HEAD`, the index and the refs (a
/// commit, a stage, a branch move, a fetch), the config (a branch's upstream, the excludes
/// file, a diff driver), and `info/exclude` and `info/attributes` (the untracked list and the
/// binary verdict). Objects, logs, the index's lock file and the turn snapshot's temporary
/// index cannot, and the refresh itself writes some of them.
fn path_affects_review(path: &Path, dirs: &GitDirs) -> bool {
    let in_git = path.strip_prefix(&dirs.git).ok();
    let in_common = path.strip_prefix(&dirs.common).ok();
    if in_git.is_none() && in_common.is_none() {
        return true;
    }
    let named = |rel: &Path, names: &[&str]| names.iter().any(|name| rel == Path::new(name));
    in_git.is_some_and(|rel| named(rel, &["HEAD", "index", "config.worktree"]) || is_ref(rel))
        || in_common.is_some_and(|rel| {
            named(rel, &["packed-refs", "config", "info/exclude", "info/attributes"]) || is_ref(rel)
        })
}

/// A path under `refs/`, which includes a ref's lock file.
fn is_ref(rel: &Path) -> bool {
    rel.starts_with("refs") && rel != Path::new("refs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, Flag, ModifyKind};

    fn plain() -> GitDirs {
        GitDirs { git: "/w/.git".into(), common: "/w/.git".into() }
    }

    fn linked() -> GitDirs {
        GitDirs { git: "/main/.git/worktrees/feature".into(), common: "/main/.git".into() }
    }

    fn affects(path: &str, dirs: &GitDirs) -> bool {
        path_affects_review(Path::new(path), dirs)
    }

    fn watch_with(watching: bool, changed: bool) -> WorktreeWatch {
        WorktreeWatch {
            shared: Arc::new(Shared {
                watching: AtomicBool::new(watching),
                lapsed: AtomicBool::new(false),
                changed: AtomicBool::new(changed),
            }),
            fallback: Duration::from_secs(30),
            last_full: Instant::now(),
            _stop: None,
        }
    }

    fn modify(path: &str) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(path.into())
    }

    #[test]
    fn a_worktree_file_always_affects_the_review() {
        assert!(affects("/w/src/main.rs", &plain()));
        assert!(affects("/w/target/debug/build.log", &plain()), "ignored files list too");
        assert!(affects("/linked/src/main.rs", &linked()));
    }

    #[test]
    fn only_head_the_index_and_the_refs_in_a_git_dir_affect_the_review() {
        let dirs = plain();
        for path in
            ["/w/.git/HEAD", "/w/.git/index", "/w/.git/refs/heads/main", "/w/.git/packed-refs"]
        {
            assert!(affects(path, &dirs), "{path} moves what the review diffs against");
        }
        assert!(affects("/w/.git/refs/heads/main.lock", &dirs), "a ref's lock is its update");
        assert!(affects("/w/.git/info/exclude", &dirs), "an exclude rule moves the untracked list");
        assert!(affects("/w/.git/info/attributes", &dirs), "an attribute moves a binary verdict");
        assert!(affects("/w/.git/config", &dirs), "the config names upstreams and excludes");
        for path in [
            "/w/.git/objects/ab/cdef",
            "/w/.git/logs/HEAD",
            "/w/.git/index.lock",
            "/w/.git/reviewr-turn-index",
            "/w/.git/reviewr-turn-index.lock",
            "/w/.git/COMMIT_EDITMSG",
            "/w/.git/refs",
        ] {
            assert!(!affects(path, &dirs), "{path} changes nothing a refresh shows");
        }
    }

    #[test]
    fn a_linked_worktree_ignores_its_siblings_and_reads_shared_refs() {
        let dirs = linked();
        assert!(affects("/main/.git/worktrees/feature/HEAD", &dirs));
        assert!(affects("/main/.git/worktrees/feature/index", &dirs));
        assert!(
            affects("/main/.git/worktrees/feature/refs/worktree/reviewr/base", &dirs),
            "a base pick from another pane of this worktree lands here"
        );
        assert!(affects("/main/.git/refs/remotes/origin/main", &dirs), "a fetch moves the base");
        assert!(affects("/main/.git/packed-refs", &dirs));
        assert!(affects("/main/.git/info/exclude", &dirs), "the shared exclude file counts");
        assert!(affects("/main/.git/info/attributes", &dirs));
        assert!(affects("/main/.git/config", &dirs), "the shared config counts");
        assert!(affects("/main/.git/worktrees/feature/config.worktree", &dirs), "and our own");
        assert!(!affects("/main/.git/worktrees/other/config.worktree", &dirs), "a sibling's not");
        assert!(!affects("/main/.git/info/refs", &dirs), "the rest of info/ does not count");
        assert!(!affects("/main/.git/worktrees/other/index", &dirs), "a sibling's stage");
        assert!(!affects("/main/.git/worktrees/other/HEAD", &dirs), "a sibling's commit");
        assert!(!affects("/main/.git/HEAD", &dirs), "the main worktree's HEAD is not ours");
        assert!(!affects("/main/.git/index", &dirs), "nor its index");
        assert!(!affects("/main/.git/objects/ab/cdef", &dirs));
    }

    #[test]
    fn a_read_never_counts_as_a_change() {
        let read = Event::new(EventKind::Access(AccessKind::Read)).add_path("/w/a.rs".into());
        assert!(!event_affects_review(&read, &plain()));
        assert!(event_affects_review(&modify("/w/a.rs"), &plain()), "a write does");
    }

    #[test]
    fn a_report_that_cannot_say_what_changed_counts_as_a_change() {
        let dirs = plain();
        let rescan =
            Event::new(EventKind::Other).add_path("/w/.git/objects".into()).set_flag(Flag::Rescan);
        assert!(event_affects_review(&rescan, &dirs), "a rescan counts whatever its path");
        let bare = Event::new(EventKind::Create(CreateKind::File));
        assert!(event_affects_review(&bare, &dirs), "so does a pathless report");
    }

    #[test]
    fn a_watcher_error_turns_every_later_poll_into_a_refresh() {
        let mut watch = watch_with(true, false);
        let start = watch.last_full;
        record(&watch.shared, &Ok(modify("/w/.git/objects/ab/cdef")), &plain());
        assert!(
            !watch.poll_due(start + Duration::from_secs(2)),
            "an ignored report changes nothing"
        );
        record(&watch.shared, &Err(notify::Error::generic("inotify watch limit")), &plain());
        assert!(!watch.is_watching(), "the watcher is no longer trusted");
        assert!(watch.poll_due(start + Duration::from_secs(4)));
        assert!(watch.poll_due(start + Duration::from_secs(6)), "for good, not once");
        record(&watch.shared, &Ok(modify("/w/a.rs")), &plain());
        assert!(watch.shared.changed.load(Ordering::SeqCst), "a later report still records");
    }

    #[test]
    fn a_report_counts_when_any_of_its_paths_does() {
        let both = Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path("/w/.git/objects/ab/cdef".into())
            .add_path("/w/.git/index".into());
        assert!(event_affects_review(&both, &plain()));
        assert!(!event_affects_review(&modify("/w/.git/objects/ab/cdef"), &plain()));
    }

    #[test]
    fn a_git_dir_is_watched_only_when_no_other_root_holds_it() {
        assert_eq!(watch_roots(Path::new("/w"), &plain()), [PathBuf::from("/w")]);
        assert_eq!(
            watch_roots(Path::new("/linked"), &linked()),
            [PathBuf::from("/linked"), PathBuf::from("/main/.git")],
            "the common dir holds the linked worktree's git dir"
        );
        let separate = GitDirs { git: "/store/repo.git".into(), common: "/store/repo.git".into() };
        assert_eq!(
            watch_roots(Path::new("/w"), &separate),
            [PathBuf::from("/w"), PathBuf::from("/store/repo.git")]
        );
    }

    #[test]
    fn a_quiet_watched_worktree_skips_the_poll_until_the_fallback() {
        let mut watch = watch_with(true, false);
        let start = watch.last_full;
        assert!(!watch.poll_due(start + Duration::from_secs(2)));
        assert!(!watch.poll_due(start + Duration::from_secs(29)));
        assert!(watch.poll_due(start + Duration::from_secs(30)), "the fallback refreshes");
        assert!(
            !watch.poll_due(start + Duration::from_secs(32)),
            "the fallback restarts its interval"
        );
    }

    #[test]
    fn a_reported_change_refreshes_the_next_poll_once() {
        let mut watch = watch_with(true, true);
        let start = watch.last_full;
        assert!(watch.poll_due(start + Duration::from_secs(2)));
        assert!(!watch.poll_due(start + Duration::from_secs(4)), "the poll cleared the change");
        watch.shared.changed.store(true, Ordering::SeqCst);
        assert!(watch.poll_due(start + Duration::from_secs(6)));
    }

    #[test]
    fn without_a_running_watcher_every_poll_refreshes() {
        let mut watch = watch_with(false, false);
        let start = watch.last_full;
        assert!(watch.poll_due(start + Duration::from_secs(2)));
        assert!(watch.poll_due(start + Duration::from_secs(4)));
    }
}
