//! The plugin's pane actions and event hook, run as `herdr-reviewr --action <name>`:
//!
//! - `toggle` opens a reviewr pane, or closes every one if any is open.
//! - `open` opens a reviewr pane, and is a no-op if one is open.
//! - `close` closes every reviewr pane, and is a no-op if none is.
//! - `auto-open` is the worktree workspace-birth hook, gated by `auto_open` and placement.
//!
//! A reviewr pane is any pane whose foreground runs the review UI, read live per pane. The
//! `reviewr` label is display only and never read. There is no state file. An action refuses
//! loudly (exit 1, one `reviewr:` line on stderr) and reports a success on stdout. A refused
//! event stays silent, except for a config error, which goes to stderr for herdr's plugin log.

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::{PluginConfig, PluginConfigError, TogglePlacement};
use crate::herdr::{self, HerdrError, PaneList, Process, ProcessInfo};
use crate::logln;
use crate::proc::program_name;

/// A run of the binary that is not the review UI, read from its arguments after argv\[0\].
///
/// `main` dispatches on this, and [`is_review_ui`] reads every observed process through it.
/// That makes "a flag run never counts as the review UI, so it never runs the review UI
/// either" one rule: a non-UI flag added here is dispatched and excluded at once.
#[derive(Debug, PartialEq, Eq)]
pub enum NonUiRun {
    /// `--resolve-plugin-config`: print the normalized plugin config.
    ResolvePluginConfig,
    /// `--action <name>`. The name is `None` when the flag ends argv.
    Action(Option<String>),
}

impl NonUiRun {
    /// The non-UI run `args` asks for, recognized anywhere in argv, or `None` for the review
    /// UI. UI flags such as `--base` and a repo path never make a run non-UI.
    pub fn from_args<S: AsRef<OsStr>>(args: &[S]) -> Option<Self> {
        let args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
        if args.contains(&OsStr::new("--resolve-plugin-config")) {
            return Some(Self::ResolvePluginConfig);
        }
        let at = args.iter().position(|arg| *arg == "--action")?;
        Some(Self::Action(args.get(at + 1).map(|name| name.to_string_lossy().into_owned())))
    }
}

/// One plugin action. The manifest names each with the same spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Toggle,
    Open,
    Close,
    AutoOpen,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "toggle" => Some(Self::Toggle),
            "open" => Some(Self::Open),
            "close" => Some(Self::Close),
            "auto-open" => Some(Self::AutoOpen),
            _ => None,
        }
    }
}

/// Why an action stopped short.
#[derive(Debug)]
enum Stop {
    /// The plugin config is invalid. Loud on every action, the event included, so the error
    /// reaches herdr's plugin log.
    Config(PluginConfigError),
    /// The action cannot proceed. Loud for an explicit action, silent for the event.
    Refused(String),
}

fn refused(why: impl Into<String>) -> Stop {
    Stop::Refused(why.into())
}

/// Run the action `name` and return the process exit code.
pub fn run(name: Option<&str>) -> i32 {
    crate::log::init();
    let Some(action) = name.and_then(Action::parse) else {
        eprintln!(
            "reviewr: unknown action '{}' (toggle | open | close | auto-open)",
            name.unwrap_or_default()
        );
        return 1;
    };
    match act(action) {
        Ok(None) => 0,
        Ok(Some(line)) => {
            println!("reviewr: {line}");
            0
        }
        Err(Stop::Config(error)) => {
            eprintln!("reviewr: {error}");
            1
        }
        Err(Stop::Refused(why)) if action == Action::AutoOpen => {
            logln!("auto-open refused: {why}");
            0
        }
        Err(Stop::Refused(why)) => {
            eprintln!("reviewr: {why}");
            1
        }
    }
}

/// One action, step by step. `Ok` holds the success line, if the action reports one.
fn act(action: Action) -> Result<Option<String>, Stop> {
    // The whole plugin config validates before any workspace read or pane write, so every
    // plugin entry point shares exactly one contract.
    let dir = crate::config::resolve_config_dir(herdr::plugin_config_dir);
    let config = crate::config::plugin_config(dir.as_deref()).map_err(Stop::Config)?;

    #[cfg(unix)]
    repoint_launch_links();

    let event = (action == Action::AutoOpen)
        .then(|| var("HERDR_PLUGIN_EVENT_JSON"))
        .flatten()
        .map(|json| serde_json::from_str::<Event>(&json).unwrap_or_default());

    // Event policy gates the event alone: explicit actions ignore it. This sits after
    // validation but before any workspace or pane read, so a disabled event does no work.
    if action == Action::AutoOpen {
        if !config.auto_open()
            || !matches!(config.toggle_placement(), TogglePlacement::Split | TogglePlacement::Tab)
        {
            return Ok(None);
        }
        // `worktree.opened` also fires when its workspace is already live. That is a
        // focus/open request, not a workspace birth: never resurrect a reviewr pane the user
        // closed there.
        if event.as_ref().is_some_and(|event| event.data.already_open == Some(true)) {
            return Ok(None);
        }
    }

    let target = Target::read(action, event);
    let Some(ws) = target.ws.as_deref() else {
        return Err(refused("no workspace context (invoke from inside herdr)"));
    };

    // One pane-list snapshot serves the whole run. A failed or unreadable listing must not
    // read as "no reviewr pane": that would stack a duplicate on toggle and false-succeed a
    // close.
    let panes =
        PaneList::of(ws).map_err(|_| refused(format!("herdr pane list failed for {ws}")))?;
    let existing = reviewr_panes(&panes)
        .ok_or_else(|| refused(format!("herdr pane process-info failed in {ws}")))?;

    if !existing.is_empty() {
        return match action {
            Action::Close | Action::Toggle => close_all(&existing, ws).map(Some),
            Action::Open => Ok(Some(format!("already open ({}) in {ws}", existing.join(" ")))),
            Action::AutoOpen => Ok(None),
        };
    }
    if action == Action::Close {
        return Ok(Some(format!("nothing open in {ws}")));
    }
    let line = open(action, &config, &target, ws, &panes)?;
    // The event reports nothing on success either.
    Ok((action != Action::AutoOpen).then_some(line))
}

/// A non-empty environment variable. herdr leaves context variables unset or empty alike.
fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

/// The action context herdr hands an action (`HERDR_PLUGIN_CONTEXT_JSON`).
#[derive(Debug, Default, Deserialize)]
struct Context {
    focused_pane_id: Option<String>,
    focused_pane_cwd: Option<String>,
    workspace_cwd: Option<String>,
}

/// The `worktree.created` / `worktree.opened` payload (`HERDR_PLUGIN_EVENT_JSON`). Every
/// field is optional: a payload missing one targets nothing, and the event then refuses.
#[derive(Debug, Default, Deserialize)]
struct Event {
    #[serde(default)]
    data: EventData,
}

#[derive(Debug, Default, Deserialize)]
struct EventData {
    workspace: Option<EventWorkspace>,
    worktree: Option<EventWorktree>,
    already_open: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct EventWorkspace {
    workspace_id: Option<String>,
    worktree: Option<EventCheckout>,
}

#[derive(Debug, Deserialize)]
struct EventCheckout {
    checkout_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventWorktree {
    path: Option<String>,
    open_workspace_id: Option<String>,
}

/// Where an action acts.
#[derive(Debug)]
struct Target {
    ws: Option<String>,
    /// The pane a split or zoomed open attaches to.
    pane: Option<String>,
    /// The launch cwd to review.
    cwd: Option<String>,
    /// The focused pane, whose live cwd a manual open prefers over `cwd`.
    focused: Option<String>,
}

impl Target {
    fn read(action: Action, event: Option<Event>) -> Self {
        let context = var("HERDR_PLUGIN_CONTEXT_JSON")
            .and_then(|json| serde_json::from_str::<Context>(&json).ok())
            .unwrap_or_default();
        let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
        // The events fire without a focused pane: target the fresh workspace from their
        // payload. The `worktree` fields are compatible fallbacks (`docs/herdr-api-notes.md`).
        if let Some(Event { data }) = event {
            let (workspace, worktree) = (data.workspace, data.worktree);
            let ws = workspace.as_ref().and_then(|w| nonempty(w.workspace_id.clone()));
            let cwd = workspace.and_then(|w| w.worktree).and_then(|w| nonempty(w.checkout_path));
            let (fallback_ws, fallback_cwd) = worktree
                .map_or((None, None), |w| (nonempty(w.open_workspace_id), nonempty(w.path)));
            return Self {
                ws: ws.or(fallback_ws),
                pane: None,
                cwd: cwd.or(fallback_cwd),
                focused: None,
            };
        }
        Self {
            ws: var("HERDR_WORKSPACE_ID"),
            pane: var("HERDR_PANE_ID"),
            cwd: nonempty(context.focused_pane_cwd).or(nonempty(context.workspace_cwd)),
            // The event keeps its payload's cwd, never a focused pane's.
            focused: nonempty(context.focused_pane_id).filter(|_| action != Action::AutoOpen),
        }
    }
}

/// The workspace's reviewr panes, in listing order: any tab, any placement, however they were
/// launched. The per-pane reads run concurrently, so the sweep costs one process-info round
/// trip of wall clock, not one per pane. `None` when any read failed, which the caller refuses
/// like a failed pane list.
fn reviewr_panes(panes: &PaneList) -> Option<Vec<&str>> {
    thread::scope(|scope| {
        let probes: Vec<_> = panes
            .panes
            .iter()
            .map(|entry| {
                let pane = entry.pane_id.as_str();
                (pane, scope.spawn(move || runs_review_ui(pane)))
            })
            .collect();
        let mut existing = Vec::new();
        for (pane, probe) in probes {
            // A probe that panicked never settled, so it refuses like a failed read.
            match probe.join() {
                Ok(Ok(true)) => existing.push(pane),
                Ok(Ok(false)) => {}
                Ok(Err(_)) | Err(_) => return None,
            }
        }
        Some(existing)
    })
}

/// Whether pane `pane` runs the review UI. A pane the read reports gone exited between the
/// list and this read, and converges like any observed-then-exited pane. Any other failed read
/// is an error.
fn runs_review_ui(pane: &str) -> Result<bool, HerdrError> {
    match ProcessInfo::of(pane) {
        Ok(info) => Ok(info.foreground_processes.iter().any(is_review_ui)),
        Err(error) if error.pane_gone() => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether `process` is the review UI: its executable is `herdr-reviewr`, and its argv asks for
/// no [`NonUiRun`]. A wrapped launch (`cargo run`) counts through its child. The executable
/// name in `argv0` or `argv[0]` decides, never `name`, which is a rewritable process title
/// (`docs/herdr-api-notes.md`).
fn is_review_ui(process: &Process) -> bool {
    let argv = process.argv.as_deref().unwrap_or_default();
    let named =
        process.argv0.iter().chain(argv.first()).any(|exe| program_name(exe) == "herdr-reviewr");
    named && NonUiRun::from_args(argv.get(1..).unwrap_or_default()).is_none()
}

/// Close every pane in `existing`, with plain `pane close` (see [`herdr::close_pane`]).
///
/// A close refused because the pane is gone lost a benign race: the pane exited between the read
/// and the close, the same end state, so the sweep still converges. A close failing any other
/// way names a pane that may still be running, so the sweep finishes the rest and then refuses
/// rather than reporting that pane closed.
fn close_all(existing: &[&str], ws: &str) -> Result<String, Stop> {
    let mut closed = Vec::new();
    let mut failed = Vec::new();
    for &pane in existing {
        match herdr::close_pane(pane) {
            Ok(()) => closed.push(pane),
            Err(error) if error.pane_gone() => closed.push(pane),
            Err(_) => failed.push(pane),
        }
    }
    if !failed.is_empty() {
        return Err(refused(format!("herdr pane close failed for {} in {ws}", failed.join(" "))));
    }
    Ok(format!("closed {} in {ws}", closed.join(" ")))
}

/// How long an open waits for its new pane to read as reviewr. herdr caches its Windows process
/// snapshot for 250 ms, so a just-opened pane can read empty, and a toggle that returned then
/// would let the next toggle open a second pane instead of closing this one. Past the bound
/// the open reports success anyway: the pane is open, only its read lags.
const VISIBLE_BOUND: Duration = Duration::from_secs(1);

/// The pause between two reads of the new pane while an open waits.
const VISIBLE_POLL: Duration = Duration::from_millis(50);

/// Open a reviewr pane in `ws` and return the success line.
fn open(
    action: Action,
    config: &PluginConfig,
    target: &Target,
    ws: &str,
    panes: &PaneList,
) -> Result<String, Stop> {
    // Prefer the focused pane's live `foreground_cwd`, read from the pane-list snapshot already
    // in hand, over the context's launch cwd (the launch-vs-live split is in
    // docs/herdr-api-notes.md). The live cwd wins only inside a repo.
    let live = target
        .focused
        .as_deref()
        .and_then(|focused| panes.pane(focused))
        .and_then(|entry| entry.foreground_cwd.clone())
        .filter(|cwd| !cwd.is_empty());
    let cwd = match (&live, &target.cwd) {
        (Some(live), _) if is_repo(live) => live,
        (_, Some(cwd)) if is_repo(cwd) => cwd,
        // Name every candidate the check rejected, or a refusal over an inspected but unusable
        // live cwd would read as if no directory was ever tried.
        _ => {
            let live = live.map(|live| format!(" (live cwd '{live}')")).unwrap_or_default();
            let cwd = target.cwd.as_deref().unwrap_or("<no cwd>");
            return Err(refused(format!("not a git repo: '{cwd}'{live}")));
        }
    };

    let plugin = var("HERDR_PLUGIN_ID").unwrap_or_else(|| "persiyanov.reviewr".to_owned());
    let placement = config.toggle_placement();
    let mut args = vec!["--plugin", &plugin, "--entrypoint", "pane", "--placement"];
    args.push(placement.as_str());
    // A split or zoomed open attaches to the focused pane, else the workspace's first pane.
    match placement {
        TogglePlacement::Split | TogglePlacement::Zoomed => {
            let attach = target
                .pane
                .as_deref()
                .or_else(|| panes.panes.first().map(|entry| entry.pane_id.as_str()))
                .ok_or_else(|| refused(format!("no pane to attach to in {ws}")))?;
            args.extend(["--target-pane", attach]);
            if placement == TogglePlacement::Split {
                args.extend(["--direction", config.toggle_direction().as_str()]);
            }
        }
        TogglePlacement::Tab => args.extend(["--workspace", ws]),
        TogglePlacement::Overlay => {}
    }
    // A manual open takes focus. The event never does.
    let focus = if action == Action::AutoOpen { "--no-focus" } else { "--focus" };
    args.extend(["--cwd", cwd, focus]);

    let opened =
        herdr::open_plugin_pane(&args).map_err(|_| refused("herdr plugin pane open failed"))?;

    // A tab open lands in a fresh tab that herdr labels with a bare index: name it after the
    // plugin so the tab bar reads "reviewr". Cosmetic, so a failed rename never fails an open
    // that already succeeded.
    if placement == TogglePlacement::Tab
        && let Some(tab) = opened.tab_id.as_deref().filter(|tab| !tab.is_empty())
    {
        let _ = herdr::rename_tab(tab, "reviewr");
    }

    wait_until_visible(&opened.pane_id);
    Ok(format!("opened {} ({}) in {ws}", opened.pane_id, placement.as_str()))
}

fn is_repo(dir: &str) -> bool {
    crate::git::toplevel(Path::new(dir)).is_some()
}

/// Return once pane `pane` reads as a reviewr pane, or once [`VISIBLE_BOUND`] has passed. A
/// sequential toggle after this one then always sees the pane it opened.
fn wait_until_visible(pane: &str) {
    let deadline = Instant::now() + VISIBLE_BOUND;
    loop {
        if runs_review_ui(pane).unwrap_or(false) {
            return;
        }
        if Instant::now() >= deadline {
            logln!("opened pane {pane} not visible as reviewr after {VISIBLE_BOUND:?}");
            return;
        }
        thread::sleep(VISIBLE_POLL);
    }
}

/// Re-point the stable launch paths at the live plugin root.
///
/// They track the root from here, not from the install step: the build step runs in a staging
/// checkout that herdr renames afterwards, so only a runtime invocation knows the real root.
/// Best effort, so this never fails an action, and it never replaces anything but a symlink: a
/// user's own binary at the path (`cargo install --root ~/.local`) survives. Unix only, since
/// symlinks on Windows need Developer Mode or admin rights.
#[cfg(unix)]
fn repoint_launch_links() {
    use std::os::unix::fs::PermissionsExt;

    let (Some(root), Some(home)) = (env::var_os("HERDR_PLUGIN_ROOT"), dirs::home_dir()) else {
        return;
    };
    if root.is_empty() {
        return;
    }
    let binary = Path::new(&root).join("bin").join("herdr-reviewr");
    let executable = std::fs::metadata(&binary)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    if !executable {
        return;
    }
    let state_bin = home.join(".local/state/herdr/plugins/persiyanov.reviewr/bin");
    let local_bin = home.join(".local/bin");
    // `~/.local/bin` only when it already exists: reviewr never creates a PATH directory.
    let dirs = [Some(state_bin), local_bin.is_dir().then_some(local_bin)];
    for dir in dirs.into_iter().flatten() {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let link = dir.join("herdr-reviewr");
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let _ = std::fs::remove_file(&link);
            }
            Ok(_) => continue,
            Err(_) => {}
        }
        let _ = std::os::unix::fs::symlink(&binary, &link);
    }
}

#[cfg(test)]
mod tests {
    use super::NonUiRun;

    #[test]
    fn a_non_ui_flag_is_recognized_anywhere_in_argv() {
        assert_eq!(
            NonUiRun::from_args(&["--some-future-arg", "--resolve-plugin-config"]),
            Some(NonUiRun::ResolvePluginConfig)
        );
        assert_eq!(
            NonUiRun::from_args(&["/repo", "--action", "toggle"]),
            Some(NonUiRun::Action(Some("toggle".into())))
        );
        assert_eq!(NonUiRun::from_args(&["--action"]), Some(NonUiRun::Action(None)));
        // UI flags and a repo path are the review UI.
        assert_eq!(NonUiRun::from_args(&["--base", "main", "/repo"]), None);
    }
}
