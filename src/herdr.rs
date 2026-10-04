//! herdr host integration: resolve the agent pane to send to, sample the agents turn
//! tracking watches, ask herdr for the plugin config directory, stamp/clear the
//! pane's cosmetic `reviewr` label, and the pane reads and writes the plugin actions
//! (`crate::actions`) are made of.
//!
//! Uses the herdr CLI via `$HERDR_BIN_PATH`, except the send, which is one request over herdr's
//! socket API at `$HERDR_SOCKET_PATH` ([`send_text`]). The two agent readers
//! ask different questions and neither narrows the other: [`send_target`] resolves candidates
//! from the reviewr pane's herdr workspace, while [`agent_samples`] reports every agent and lets
//! the caller decide membership by worktree. Browsing and the clipboard export never come
//! through here.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::logln;
use crate::turn::Status;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AgentListResponse {
    result: AgentList,
}

#[derive(Debug, Deserialize)]
struct AgentList {
    agents: Vec<AgentPane>,
}

/// One entry of `herdr agent list`. The picker-facing fields are optional: herdr 0.7.5 omits
/// `name`, `display_agent`, and `state_labels` entirely until something sets them, and
/// `herdr agent rename --clear` leaves `name` present and null. Both parse to `None`. The
/// identity fields stay required, so a payload missing `pane_id` fails the parse loudly
/// instead of minting an unaddressable send target.
///
/// `agent_status` is kept as herdr spelled it, not as the [`Status`] it parses to: the picker
/// row shows the spelling and looks its label up by it, so a state herdr adds must survive a
/// round trip reviewr does not understand.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct AgentPane {
    agent: Option<String>,
    agent_status: String,
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    /// Where the agent works. Turn tracking resolves it to a git top level to decide which
    /// worktree the agent belongs to.
    cwd: Option<String>,
    name: Option<String>,
    display_agent: Option<String>,
    state_labels: Option<HashMap<String, String>>,
}

/// One picker row: the pane the send addresses, and the three parts the row shows
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub pane_id: String,
    pub name: String,
    pub state: String,
    pub tab: String,
}

/// What `Send` does with the agents herdr reports. A refusal is the
/// `Err` of [`send_target`], so zero agents and a failed enumeration land in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendTarget {
    /// Exactly one agent. The send goes straight to it, with no picker.
    One(AgentChoice),
    /// Several agents, in herdr's own order. The picker opens over them.
    Many(Vec<AgentChoice>),
}

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// How a herdr call failed, classified so a caller can tell a benign race from a real failure.
#[derive(Debug, PartialEq, Eq)]
pub enum HerdrError {
    /// herdr could not be run at all.
    Unanswered,
    /// herdr ran and exited non-zero. Holds the `error.code` of the JSON envelope it wrote to
    /// stderr, when it wrote one (`docs/herdr-api-notes.md`).
    Refused(Option<String>),
    /// herdr exited 0, but its answer is missing the shape the call documents. A shape
    /// failure is never read as an empty answer.
    Unreadable,
}

impl HerdrError {
    /// The addressed pane no longer exists: it exited between an earlier read and this call.
    pub fn pane_gone(&self) -> bool {
        matches!(self, Self::Refused(Some(code)) if code == "pane_not_found")
    }
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unanswered => write!(f, "herdr could not run"),
            Self::Refused(Some(code)) => write!(f, "herdr refused: {code}"),
            Self::Refused(None) => write!(f, "herdr refused"),
            Self::Unreadable => write!(f, "herdr answered in an unknown shape"),
        }
    }
}

impl std::error::Error for HerdrError {}

/// Run a herdr subcommand and return its stdout, or the classified failure.
///
/// Nothing shows this error to a reviewer: every caller either replaces it with a sentence of its
/// own or drops it. So the whole of it, the argv and herdr's JSON error envelope, goes to the log
/// and only there.
fn call(args: &[&str]) -> Result<String, HerdrError> {
    let out = match crate::proc::command(herdr_bin()).args(args).output() {
        Ok(out) => out,
        Err(e) => {
            logln!("herdr {args:?} could not run: {e}");
            return Err(HerdrError::Unanswered);
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        logln!("herdr {args:?} failed: {}", stderr.trim());
        return Err(HerdrError::Refused(error_code(&stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `error.code` of the JSON envelope a failed herdr call writes to stderr. Read line by
/// line, so an advisory line printed beside the envelope cannot hide it.
fn error_code(stderr: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Envelope {
        error: ErrorBody,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
    }
    stderr
        .lines()
        .find_map(|line| serde_json::from_str::<Envelope>(line.trim()).ok())
        .map(|envelope| envelope.error.code)
}

/// [`call`] for the review UI's own calls, where any failure is just a failure. A herdr that
/// could not run becomes [`Refusal::Unanswered`], which the app words for the reviewer.
fn herdr(args: &[&str]) -> Result<String> {
    match call(args) {
        Ok(out) => Ok(out),
        // No herdr to ask is herdr not answering, whichever call it was.
        Err(HerdrError::Unanswered) => Err(Refusal::Unanswered.into()),
        Err(_) => bail!("herdr refused"),
    }
}

/// The `result` of a herdr JSON answer, parsed as `T`. Anything else is
/// [`HerdrError::Unreadable`].
fn answer<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, HerdrError> {
    #[derive(Deserialize)]
    struct Answer<T> {
        result: T,
    }
    serde_json::from_str::<Answer<T>>(json).map(|answer| answer.result).map_err(|error| {
        logln!("herdr answer unreadable: {error}");
        HerdrError::Unreadable
    })
}

/// One `herdr pane list` snapshot of a workspace.
#[derive(Debug, Deserialize)]
pub struct PaneList {
    pub panes: Vec<PaneEntry>,
}

/// One pane in a [`PaneList`]. `pane_id` is required: an entry without one could never be
/// addressed, so the listing fails to parse instead.
#[derive(Debug, Deserialize)]
pub struct PaneEntry {
    pub pane_id: String,
    /// The live foreground process's cwd, which can differ from the pane's launch cwd
    /// (`docs/herdr-api-notes.md`).
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

impl PaneList {
    /// The panes in workspace `ws`.
    pub fn of(ws: &str) -> Result<Self, HerdrError> {
        answer(&call(&["pane", "list", "--workspace", ws])?)
    }

    /// The entry for pane `pane`, if the snapshot lists it.
    pub fn pane(&self, pane: &str) -> Option<&PaneEntry> {
        self.panes.iter().find(|entry| entry.pane_id == pane)
    }

    /// The `label` of pane `pane`. An absent key, an empty label, and an unknown pane all read
    /// as no label.
    fn label(&self, pane: &str) -> Option<&str> {
        self.pane(pane)?.label.as_deref().filter(|label| !label.is_empty())
    }
}

/// One `herdr pane process-info` answer: the processes herdr reports in the pane's foreground.
/// On macOS and Linux that is the foreground process group. On Windows it is one process, the
/// topmost recognized agent or else the pane's root process.
#[derive(Debug, Deserialize)]
pub struct ProcessInfo {
    /// Required, and what marks an answer as a process-info answer at all: an answer without
    /// it is a shape failure, never "no processes".
    pub pane_id: String,
    /// herdr omits the key when the list is empty, so an absent key is zero processes. On
    /// Windows a just-opened pane answers that way until herdr's 250 ms process snapshot
    /// catches up (`docs/herdr-api-notes.md`).
    #[serde(default)]
    pub foreground_processes: Vec<Process>,
}

/// One foreground process. `name` is left out on purpose: it is a rewritable process title,
/// so only the executable identifies a process (`docs/herdr-api-notes.md`).
#[derive(Debug, Deserialize)]
pub struct Process {
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
}

impl ProcessInfo {
    /// The foreground processes of pane `pane`.
    pub fn of(pane: &str) -> Result<Self, HerdrError> {
        Self::parse(&call(&["pane", "process-info", "--pane", pane])?)
    }

    fn parse(json: &str) -> Result<Self, HerdrError> {
        #[derive(Deserialize)]
        struct Result {
            process_info: ProcessInfo,
        }
        answer::<Result>(json).map(|result| result.process_info)
    }
}

/// The pane a `herdr plugin pane open` created.
#[derive(Debug, Deserialize)]
pub struct OpenedPane {
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: Option<String>,
}

/// Open one of a plugin's panes. `args` follows `plugin pane open` on the command line.
pub fn open_plugin_pane(args: &[&str]) -> Result<OpenedPane, HerdrError> {
    #[derive(Deserialize)]
    struct Result {
        plugin_pane: PluginPane,
    }
    #[derive(Deserialize)]
    struct PluginPane {
        pane: OpenedPane,
    }
    let call_args = [&["plugin", "pane", "open"], args].concat();
    let opened = answer::<Result>(&call(&call_args)?)?.plugin_pane.pane;
    if opened.pane_id.is_empty() {
        return Err(HerdrError::Unreadable);
    }
    Ok(opened)
}

/// Close pane `pane` with plain `pane close`, which reaches any pane by id. `plugin pane close`
/// only reaches panes in herdr's in-memory plugin-pane registry, which forgets them on a restart
/// and never holds a layout-launched one (`docs/herdr-api-notes.md`).
pub fn close_pane(pane: &str) -> Result<(), HerdrError> {
    call(&["pane", "close", pane]).map(drop)
}

/// Set tab `tab`'s label.
pub fn rename_tab(tab: &str, label: &str) -> Result<(), HerdrError> {
    call(&["tab", "rename", tab, label]).map(drop)
}

/// How long a startup or exit path waits for a herdr answer before moving on. The call keeps
/// running on its own thread — only the wait is bounded — so a wedged herdr costs at most
/// this once and never wedges reviewr with it: not the first paint, not the event loop's
/// entry, and not the shell prompt after exit.
const ANSWER_BOUND: Duration = Duration::from_secs(2);

/// How long a herdr answer may take before the caller signals the wait. Under this, the
/// answer is effectively instant and nothing flashes; over it, the caller says what it is
/// waiting on, so a slow answer never swaps the screen silently
/// (`policies/ux-responsiveness.md`).
const SIGNAL_DELAY: Duration = Duration::from_millis(150);

/// Run a herdr subcommand on its own thread and hand back the channel its answer lands on.
/// Dropping the receiver makes the call fire-and-forget; the thread still reaps the child
/// either way, and a failure logs inside [`herdr`] as usual.
fn herdr_on_thread(args: Vec<String>) -> mpsc::Receiver<Result<String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = tx.send(herdr(&refs));
    });
    rx
}

/// Stamp our own pane's cosmetic `reviewr` label — but only when the pane carries no label,
/// so a name the user gave their pane survives running reviewr in it (reviewr supplies the default name, never overrides one). Display only:
/// the actions and the event identify a reviewr pane by its foreground process, never this
/// label, so a failed read or write just logs — and nothing waits on it, so a hung herdr
/// cannot sit between the first paint and the event loop. Without a pane id — outside
/// herdr — a no-op.
pub fn label_pane() {
    let (Ok(ws), Ok(pane)) = (env::var("HERDR_WORKSPACE_ID"), env::var("HERDR_PANE_ID")) else {
        return;
    };
    thread::spawn(move || {
        // An unreadable listing stamps anyway: with herdr wedged the rename fails too,
        // and both failures land in the log.
        if current_label(&ws, &pane).is_none() {
            let _ = herdr(&["pane", "rename", &pane, "reviewr"]);
        }
    });
}

/// Clear the cosmetic label on a normal exit — but only a `reviewr` label, so a name the
/// user set is never deleted. The wait is bounded:
/// this runs after the terminal is restored, and a hung herdr must not hold the shell
/// prompt hostage for a label a stale copy of which changes nothing.
pub fn clear_pane_label() {
    let (Ok(ws), Ok(pane)) = (env::var("HERDR_WORKSPACE_ID"), env::var("HERDR_PANE_ID")) else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if current_label(&ws, &pane).as_deref() == Some("reviewr") {
            let _ = herdr(&["pane", "rename", &pane, "--clear"]);
        }
        let _ = tx.send(());
    });
    if rx.recv_timeout(ANSWER_BOUND).is_err() {
        logln!("pane label clear unanswered after {ANSWER_BOUND:?}; leaving the label");
    }
}

/// Our pane's current label from `pane list`, or `None` when it has none or the listing
/// fails. Blocking — the label threads call it, never the frame loop.
fn current_label(ws: &str, pane: &str) -> Option<String> {
    PaneList::of(ws).ok()?.label(pane).map(str::to_owned)
}

/// The config directory herdr resolves for this plugin, from `herdr plugin config-dir`.
/// `None` — herdr absent, refusing, or not answering — means no config directory, never an
/// error.
pub fn plugin_config_dir() -> Option<String> {
    plugin_config_dir_with(|| ())
}

/// [`plugin_config_dir`] with a slow-answer signal: `on_slow` runs once if the answer takes
/// longer than [`SIGNAL_DELAY`], so the pane can say what it is waiting on instead of
/// silently swapping a painted frame later. The pane calls this only after its first paint,
/// and the total wait stays bounded by [`ANSWER_BOUND`], so a wedged herdr degrades a
/// visible pane to the defaults instead of holding the blank grid issue #4 fixed.
pub fn plugin_config_dir_with(on_slow: impl FnOnce()) -> Option<String> {
    let rx =
        herdr_on_thread(vec!["plugin".into(), "config-dir".into(), "persiyanov.reviewr".into()]);
    let answer = if let Ok(answer) = rx.recv_timeout(SIGNAL_DELAY) {
        answer
    } else {
        on_slow();
        let Ok(answer) = rx.recv_timeout(ANSWER_BOUND.saturating_sub(SIGNAL_DELAY)) else {
            logln!("plugin config-dir unanswered after {ANSWER_BOUND:?}; no config directory");
            return None;
        };
        answer
    };
    let out = answer.ok()?;
    let dir = out.trim();
    (!dir.is_empty()).then(|| dir.to_owned())
}

/// The (workspace, pane) id pair identifying this reviewr pane in the herdr environment. There is
/// no tab here on purpose: the send scopes to the workspace and turn tracking scopes to the
/// worktree, so nothing reads `HERDR_TAB_ID` and the reviewr pane's placement changes neither.
fn agent_env() -> (Option<String>, Option<String>) {
    (env::var("HERDR_WORKSPACE_ID").ok(), env::var("HERDR_PANE_ID").ok())
}

/// The agents herdr currently lists. The one place the `agent list` call and its envelope
/// parsing live, shared by the send's pane resolution and turn tracking's sampling.
fn agent_list() -> Result<Vec<AgentPane>> {
    parse_agents(&herdr(&["agent", "list"])?)
}

/// What `Send` does: one workspace agent sends directly, several open the picker, and no
/// agent refuses. A failed enumeration refuses too, but says so rather
/// than reporting a count herdr never gave. Either refusal is the whole status line, so both
/// stay one short sentence naming the clipboard the reviewer can fall back to.
pub fn send_target() -> Result<SendTarget> {
    let (ws, me) = agent_env();
    let agents = match agent_list() {
        Ok(agents) => agents,
        Err(e) => {
            // A refusal is the whole status line, so it says the clipboard rather than herdr's
            // own wording. The cause is already in the log, with the argv `herdr` kept out of it.
            logln!("agent list failed: {e:#}");
            return Err(Refusal::Unanswered.into());
        }
    };
    // Candidacy is decided once, here: an `agent` field, our workspace, not our own pane.
    // Rows keep `agent list` order, which is herdr's own. Turn
    // tracking does not come through here: it asks where each agent works instead.
    let picked = candidates(&agents, ws.as_deref(), me.as_deref());
    match picked.len() {
        0 => Err(Refusal::NoAgent.into()),
        // The sole-agent send shows no row, so only the picker pays for the tab-label call.
        1 => Ok(SendTarget::One(picked[0].choice(&HashMap::new()))),
        _ => {
            let tabs = tab_labels(ws.as_deref());
            Ok(SendTarget::Many(picked.into_iter().map(|agent| agent.choice(&tabs)).collect()))
        }
    }
}

impl AgentPane {
    /// This pane as a picker row.
    fn choice(&self, tabs: &HashMap<String, String>) -> AgentChoice {
        AgentChoice {
            pane_id: self.pane_id.clone(),
            name: self.row_name(),
            state: self.row_state(),
            tab: tabs.get(&self.tab_id).cloned().unwrap_or_default(),
        }
    }

    /// The agent's `name`, else its `display_agent`, else its kind.
    /// A cleared name arrives as null and falls through like an absent one. The pane id is a
    /// last resort no live agent reaches, so the row and the success line always name something.
    fn row_name(&self) -> String {
        [&self.name, &self.display_agent, &self.agent]
            .into_iter()
            .flatten()
            .find(|part| !part.is_empty())
            .cloned()
            .unwrap_or_else(|| self.pane_id.clone())
    }

    /// The agent's `state_labels` entry for its state, else the state itself. Both the lookup
    /// key and the fallback are herdr's own spelling, so a state reviewr does not know still
    /// names itself on the row instead of reading `unknown`.
    fn row_state(&self) -> String {
        self.state_labels
            .as_ref()
            .and_then(|labels| labels.get(&self.agent_status))
            .filter(|label| !label.is_empty())
            .cloned()
            .unwrap_or_else(|| self.agent_status.clone())
    }

    /// The lifecycle status turn tracking reads from this pane.
    fn status(&self) -> Status {
        Status::from_wire(&self.agent_status)
    }

    /// A real agent pane other than our own — the shared gate both readers apply, so turn
    /// sampling and send targeting never drift on what counts as an agent
    /// (`../docs/herdr-api-notes.md`).
    fn is_agent_other_than(&self, me: Option<&str>) -> bool {
        self.agent.is_some() && Some(self.pane_id.as_str()) != me
    }
}

/// Tab id to tab label for one workspace. Labelling is best effort: a failed call or a
/// missing tab leaves the row's tab part empty rather than failing the send.
fn tab_labels(ws: Option<&str>) -> HashMap<String, String> {
    let Some(ws) = ws else { return HashMap::new() };
    let Ok(json) = herdr(&["tab", "list", "--workspace", ws]) else {
        return HashMap::new();
    };
    parse_tab_labels(&json).unwrap_or_default()
}

/// The documented `result.tabs` array from `herdr tab list`, as tab id → label. A tab
/// without a label is dropped, so its rows show no tab part.
fn parse_tab_labels(json: &str) -> Result<HashMap<String, String>> {
    let response: TabListResponse = serde_json::from_str(json).context("parsing tab list")?;
    Ok(response
        .result
        .tabs
        .into_iter()
        .filter_map(|tab| tab.label.map(|label| (tab.tab_id, label)))
        .collect())
}

#[derive(Debug, Deserialize)]
struct TabListResponse {
    result: TabList,
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<TabInfo>,
}

#[derive(Debug, Deserialize)]
struct TabInfo {
    tab_id: String,
    #[serde(default)]
    label: Option<String>,
}

/// The documented `result.agents` array from `herdr agent list`.
fn parse_agents(json: &str) -> Result<Vec<AgentPane>> {
    let response: AgentListResponse = serde_json::from_str(json).context("parsing agent list")?;
    Ok(response.result.agents)
}

/// One agent as turn tracking sees it: where it works, and what it is doing. Membership is
/// the caller's to decide, since only the worker knows the reviewed worktree
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSample {
    pub cwd: Option<String>,
    pub status: Status,
}

/// Every agent herdr reports, minus our own pane. Neither the tab nor the workspace narrows
/// this (see the module header). `Err` means the enumeration failed, which the caller treats
/// as "nothing changed" rather than "no agents".
pub fn agent_samples() -> Result<Vec<AgentSample>> {
    let (_, me) = agent_env();
    Ok(samples_of(agent_list()?, me.as_deref()))
}

/// The sampling rule, split out so it is testable without the CLI. Only entries carrying an
/// `agent` field count, and our own pane never does.
fn samples_of(agents: Vec<AgentPane>, me: Option<&str>) -> Vec<AgentSample> {
    agents
        .into_iter()
        .filter(|agent| agent.is_agent_other_than(me))
        .map(|agent| AgentSample { status: agent.status(), cwd: agent.cwd })
        .collect()
}

/// The real agents in workspace `ws`, ignoring our own pane `me`. Only entries carrying an
/// `agent` field count. herdr 0.7.5 already keeps non-agent panes
/// out of `agent list`, so both filters are defensive: a reviewr pane or a plain shell shows
/// up in `pane list` without an `agent` key and never here (`../docs/herdr-api-notes.md`).
fn candidates<'a>(
    agents: &'a [AgentPane],
    ws: Option<&str>,
    me: Option<&str>,
) -> Vec<&'a AgentPane> {
    let Some(ws) = ws else { return Vec::new() };
    agents
        .iter()
        .filter(|agent| agent.is_agent_other_than(me))
        .filter(|agent| agent.workspace_id == ws)
        .collect()
}

/// Why a send went nowhere. Every comment stays. The app words the reviewer's line, since it
/// knows the copy key to offer instead.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The named agent waits on a permission or confirm prompt.
    AtPrompt(String),
    /// herdr did not answer a call: it could not run, or could not list the agents.
    Unanswered,
    /// The workspace holds no agent to send to.
    NoAgent,
    /// The review is over herdr's request cap, so it cannot go as one paste.
    TooLarge,
}

/// The log's wording. The reviewer's line is the app's (`App::refusal_line`).
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::AtPrompt(name) => write!(f, "{name} is at a prompt"),
            Refusal::Unanswered => write!(f, "herdr did not answer"),
            Refusal::NoAgent => write!(f, "no agent in the workspace"),
            Refusal::TooLarge => {
                write!(f, "the review is over herdr's {MAX_REQUEST_BYTES}-byte request cap")
            }
        }
    }
}

impl std::error::Error for Refusal {}

/// Whether an agent pane can take a send right now.
#[derive(Debug, PartialEq, Eq)]
enum Readiness {
    /// The agent's input takes the paste.
    Ready,
    /// The agent is at a prompt. Holds its name, as the picker row shows it.
    Busy(String),
    /// The pane is no longer an agent herdr lists.
    Gone,
}

/// Refuse a send to an agent at a prompt, read from a fresh `agent list` at the moment of
/// sending: a prompt drops a paste, so the comments would never reach the input
/// (`docs/herdr-api-notes.md`). The read and the send are two herdr calls, so an agent can
/// still raise a prompt in between. herdr offers no atomic send-if-ready.
fn ensure_ready(pane: &str) -> Result<()> {
    let agents = match agent_list() {
        Ok(agents) => agents,
        Err(e) => {
            logln!("agent list failed before the send: {e:#}");
            return Err(Refusal::Unanswered.into());
        }
    };
    match readiness_in(&agents, pane) {
        Readiness::Ready => Ok(()),
        Readiness::Busy(name) => Err(Refusal::AtPrompt(name).into()),
        Readiness::Gone => bail!("agent pane {pane} is gone"),
    }
}

/// Only an agent at a prompt refuses: a permission or confirm prompt drops a paste. A working
/// agent takes typing mid-turn, and the paste waits in its input for the reviewer to submit,
/// which is how a review reaches a running agent.
fn readiness_in(agents: &[AgentPane], pane: &str) -> Readiness {
    match agents.iter().find(|agent| agent.pane_id == pane && agent.agent.is_some()) {
        None => Readiness::Gone,
        Some(agent) if agent.status() == Status::Blocked => Readiness::Busy(agent.row_name()),
        Some(_) => Readiness::Ready,
    }
}

/// herdr's cap on one socket request line, newline excluded: past it herdr stops reading and
/// drops the connection unanswered (`MAX_INITIAL_REQUEST_BYTES` in herdr's `src/api/server.rs`).
/// A connection carries exactly one request, so this caps the whole send.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// How long a send waits for herdr's reply. herdr gives up reading a request 5 s after the
/// connection opens (`INITIAL_REQUEST_TIMEOUT`), and its write into the pane is a channel push,
/// so past that plus [`ANSWER_BOUND`] for the answer herdr has dropped the request or is wedged.
/// Waiting less would give up on a large send herdr is still reading: the comments stay, the
/// paste lands anyway, and the next `Send` pastes the review twice.
const SEND_BOUND: Duration = Duration::from_secs(5).saturating_add(ANSWER_BOUND);

/// Write literal text into the agent pane's input, without submitting, once the agent is ready
/// for it ([`ensure_ready`]).
///
/// The text goes as one `pane.send_text` request over herdr's socket API, which writes it to the
/// pane as raw bytes. The CLI's `pane send-text` takes the text as one argument, and Windows caps
/// a command line at 32,767 characters, so a long review could not go that way
/// (`docs/herdr-api-notes.md`).
///
/// Both refusals that need no herdr run before anything is asked of it: no socket is no herdr to
/// ask, as a missing herdr binary is for every CLI call, and a review over the cap could never
/// land. Only a `result` reply is success. An error reply is herdr refusing the paste, and a
/// dropped or unanswered connection is herdr not answering, though the paste may have landed.
pub fn send_text(pane: &str, text: &str) -> Result<()> {
    let Some(socket) = env::var_os("HERDR_SOCKET_PATH") else {
        return Err(Refusal::Unanswered.into());
    };
    let request = serde_json::json!({
        "id": "reviewr:send",
        "method": "pane.send_text",
        "params": {"pane_id": pane, "text": paste_payload(text)},
    })
    .to_string();
    if request.len() > MAX_REQUEST_BYTES {
        return Err(Refusal::TooLarge.into());
    }
    ensure_ready(pane)?;
    match socket_call(socket, request) {
        Ok(()) => Ok(()),
        Err(HerdrError::Unanswered) => Err(Refusal::Unanswered.into()),
        Err(error) => Err(error.into()),
    }
}

/// One request line over herdr's socket, answered by one reply line, bounded by [`SEND_BOUND`].
/// The exchange runs on its own thread, so a wedged herdr costs the wait and never the frame
/// loop's life. The thread ends at the same deadline, except on a Windows pipe herdr holds open
/// unanswered ([`socket`]).
fn socket_call(socket: OsString, request: String) -> Result<(), HerdrError> {
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + SEND_BOUND;
    thread::spawn(move || {
        let _ = tx.send(socket::exchange(&socket, &request, deadline));
    });
    let reply = match rx.recv_timeout(SEND_BOUND) {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => {
            logln!("herdr socket call failed: {error}");
            return Err(HerdrError::Unanswered);
        }
        Err(_) => {
            logln!("herdr socket call unanswered after {SEND_BOUND:?}");
            return Err(HerdrError::Unanswered);
        }
    };
    reply_outcome(&reply)
}

/// A socket reply as a call outcome: a `result` is success, and an error envelope is a refusal
/// carrying its code. herdr echoes the same envelope it writes to stderr for a failed CLI call.
fn reply_outcome(reply: &str) -> Result<(), HerdrError> {
    if let Some(code) = error_code(reply) {
        logln!("herdr refused over the socket: {}", reply.trim());
        return Err(HerdrError::Refused(Some(code)));
    }
    answer::<serde::de::IgnoredAny>(reply).map(drop)
}

/// The socket transport, a client of herdr's `src/ipc.rs`. `HERDR_SOCKET_PATH` names a Unix
/// domain socket on unix. On Windows it names a marker file, and the named pipe is that path as
/// a namespaced local socket name (herdr's `connect_local_stream`). herdr reads one request line
/// per connection, answers it with one line, and closes.
///
/// On unix every read and write ends at the send's deadline, so a herdr that accepts and never
/// answers frees the worker thread and its descriptor once the send gives up. The connect itself
/// returns at once unless herdr stopped accepting with its whole backlog queued. A Windows named
/// pipe takes no read or write timeout (`interprocess` reports them unsupported, and herdr's own
/// client goes without), so there the deadline bounds only the wait for a free pipe instance. A
/// herdr that accepts and never answers keeps the worker thread and its pipe handle until herdr
/// closes the pipe or exits. The frame loop waits at most [`SEND_BOUND`](super::SEND_BOUND)
/// either way (`socket_call`).
mod socket {
    use std::ffi::OsStr;
    use std::io::{self, BufRead, BufReader, Write};
    use std::time::{Duration, Instant};

    /// Write `request` as one line and read the one line herdr answers.
    pub(super) fn exchange(socket: &OsStr, request: &str, deadline: Instant) -> io::Result<String> {
        let mut stream = connect(socket, deadline)?;
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        if !reply.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "herdr closed the connection without answering",
            ));
        }
        Ok(reply)
    }

    /// The time left before `deadline`, or a timeout once none is.
    fn left(deadline: Instant) -> io::Result<Duration> {
        Some(deadline.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "herdr did not answer in time"))
    }

    #[cfg(unix)]
    fn connect(socket: &OsStr, deadline: Instant) -> io::Result<Bounded> {
        Ok(Bounded { stream: std::os::unix::net::UnixStream::connect(socket)?, deadline })
    }

    /// A Unix socket connection whose every read and write waits only until the deadline.
    #[cfg(unix)]
    struct Bounded {
        stream: std::os::unix::net::UnixStream,
        deadline: Instant,
    }

    #[cfg(unix)]
    impl io::Read for Bounded {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.stream.set_read_timeout(Some(left(self.deadline)?))?;
            self.stream.read(buf)
        }
    }

    #[cfg(unix)]
    impl Write for Bounded {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.stream.set_write_timeout(Some(left(self.deadline)?))?;
            self.stream.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    /// Connect the way herdr's own client does, waiting for a free pipe instance only until the
    /// deadline. Every instance can be taken for a moment, between herdr accepting one client
    /// and opening the next.
    #[cfg(windows)]
    fn connect(
        socket: &OsStr,
        deadline: Instant,
    ) -> io::Result<interprocess::local_socket::Stream> {
        use interprocess::ConnectWaitMode;
        use interprocess::local_socket::{ConnectOptions, GenericNamespaced, prelude::*};
        let name = socket.to_string_lossy().into_owned().to_ns_name::<GenericNamespaced>()?;
        ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(left(deadline)?))
            .connect_sync()
    }
}

/// The text as herdr's own paste delivers it, framed as one bracketed paste.
///
/// `pane.send_text` writes its bytes to the pane untouched, so the newline encoding is reviewr's
/// to do: CRLF on Windows and the text unchanged elsewhere, as herdr encodes a paste of its own
/// (`prepare_paste_text_for_pty_platform`).
fn paste_payload(text: &str) -> String {
    if cfg!(windows) { pasted(&crate::export::crlf_line_breaks(text)) } else { pasted(text) }
}

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The batch as one bracketed paste event, never raw bytes: a paste inserts verbatim in any
/// input mode, where raw bytes execute as commands in a vim-style input resting in normal
/// mode. A terminator inside the batch would end the frame early and
/// hand the tail to the command interpreter. The body is rebuilt with a suffix check per
/// character, so a terminator never survives, not even one spliced together by an earlier
/// removal — and the send stays linear, where a delete-and-rescan loop is quadratic on
/// splice-heavy input and stalls the frame loop mid-send.
fn pasted(text: &str) -> String {
    let mut body = String::with_capacity(text.len());
    for ch in text.chars() {
        body.push(ch);
        if body.ends_with(PASTE_END) {
            body.truncate(body.len() - PASTE_END.len());
        }
    }
    format!("{PASTE_START}{body}{PASTE_END}")
}

/// Focus the agent pane so the reviewer can add context and submit.
pub fn focus(pane: &str) -> Result<()> {
    herdr(&["agent", "focus", pane])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AgentChoice, AgentPane, HashMap, HerdrError, Status, parse_agents, parse_tab_labels,
    };

    /// One agent entry shaped like the real `herdr agent list` output (api notes).
    fn agent(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: Some("claude".to_string()),
            agent_status: "working".to_string(),
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
            ..AgentPane::default()
        }
    }

    /// One non-agent pane as herdr 0.7.1 lists it live: `agent_status: unknown`, no `agent`
    /// field — a reviewr pane or a plain shell.
    fn non_agent_pane(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: None,
            agent_status: "unknown".to_string(),
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
            ..AgentPane::default()
        }
    }

    /// The picker-row mapping `send_target`'s Many arm applies to the workspace candidates.
    fn rows(
        agents: &[AgentPane],
        ws: Option<&str>,
        me: Option<&str>,
        tabs: &HashMap<String, String>,
    ) -> Vec<AgentChoice> {
        super::candidates(agents, ws, me).into_iter().map(|agent| agent.choice(tabs)).collect()
    }

    #[test]
    fn a_send_refuses_only_an_agent_at_a_prompt() {
        use super::Readiness::{Busy, Gone, Ready};
        let at = |status: &str| AgentPane {
            agent_status: status.into(),
            state_labels: Some(HashMap::from([("compacting".into(), "Compacting".into())])),
            ..agent("w8:p1", "w8:t1", "w8")
        };
        for (status, want) in [
            ("idle", Ready),
            ("done", Ready),
            // A working agent takes typing mid-turn: the paste waits in its input.
            ("working", Ready),
            ("unknown", Ready),
            ("compacting", Ready),
            // A prompt drops a paste.
            ("blocked", Busy("claude".into())),
        ] {
            assert_eq!(super::readiness_in(&[at(status)], "w8:p1"), want, "{status}");
        }
        assert_eq!(super::readiness_in(&[at("idle")], "w8:p9"), Gone);
        // A pane whose agent exited is listed without one: the send goes nowhere near it.
        let shell = AgentPane { agent: None, ..at("idle") };
        assert_eq!(super::readiness_in(&[shell], "w8:p1"), Gone);
    }

    #[test]
    fn sampling_keeps_every_tab_and_workspace() {
        // Turn tracking asks where an agent works, never where its pane sits, so neither the
        // reviewr pane's tab nor its workspace narrows the sample (HH-TURN-PER-WORKTREE).
        // This is what makes the `tab` placement track exactly like `split`.
        let agents = vec![
            AgentPane { cwd: Some("/w/one".into()), ..agent("w8:p1", "w8:t1", "w8") },
            AgentPane { cwd: Some("/w/two".into()), ..agent("w8:p2", "w8:t2", "w8") },
            AgentPane { cwd: Some("/w/three".into()), ..agent("w9:p1", "w9:t1", "w9") },
        ];
        let cwds: Vec<_> =
            super::samples_of(agents, None).into_iter().filter_map(|s| s.cwd).collect();
        assert_eq!(cwds, ["/w/one", "/w/two", "/w/three"]);
    }

    #[test]
    fn sampling_drops_our_own_pane_and_every_non_agent_pane() {
        let agents = vec![
            AgentPane { cwd: Some("/w/real".into()), ..agent("w3:p1", "w3:t1", "w3") },
            AgentPane { cwd: Some("/w/shell".into()), ..non_agent_pane("w3:p4", "w3:t1", "w3") },
            AgentPane { cwd: Some("/w/self".into()), ..agent("w3:p5", "w3:t1", "w3") },
        ];
        let cwds: Vec<_> =
            super::samples_of(agents, Some("w3:p5")).into_iter().filter_map(|s| s.cwd).collect();
        assert_eq!(cwds, ["/w/real"]);
    }

    #[test]
    fn a_sample_carries_the_status_tracking_folds() {
        let agents = vec![AgentPane {
            agent_status: "blocked".into(),
            cwd: Some("/w/one".into()),
            ..agent("w8:p1", "w8:t1", "w8")
        }];
        assert_eq!(super::samples_of(agents, None)[0].status, Status::Blocked);
    }

    /// One agent carrying the picker-facing fields herdr omits until something sets them.
    fn named(pane: &str, tab: &str, ws: &str, name: Option<&str>) -> AgentPane {
        AgentPane { name: name.map(str::to_string), ..agent(pane, tab, ws) }
    }

    #[test]
    fn a_row_name_prefers_the_rename_then_the_display_agent_then_the_kind() {
        // `herdr agent rename` sets `name`, which wins.
        assert_eq!(named("w8:p1", "w8:t1", "w8", Some("release-bot")).row_name(), "release-bot");
        // `--clear` leaves the key present and null, which falls through like an absent one.
        let cleared = named("w8:p1", "w8:t1", "w8", None);
        assert_eq!(cleared.row_name(), "claude");
        // With no kind either, the pane id keeps the row and the success line from going blank.
        let anonymous = AgentPane { agent: None, ..agent("w8:p1", "w8:t1", "w8") };
        assert_eq!(anonymous.row_name(), "w8:p1");
        let displayed = AgentPane {
            agent: None,
            display_agent: Some("Claude".into()),
            ..agent("w8:p1", "w8:t1", "w8")
        };
        assert_eq!(displayed.row_name(), "Claude");
    }

    #[test]
    fn a_row_state_prefers_the_state_label_over_the_wire_spelling() {
        let mut labels = HashMap::new();
        labels.insert("working".to_string(), "thinking".to_string());
        let labelled = AgentPane { state_labels: Some(labels), ..agent("w8:p1", "w8:t1", "w8") };
        assert_eq!(labelled.row_state(), "thinking");
        // herdr 0.7.5 sends no `state_labels`, so every live row falls back to the state itself.
        assert_eq!(agent("w8:p1", "w8:t1", "w8").row_state(), "working");
    }

    #[test]
    fn picker_rows_are_every_workspace_agent_in_herdr_order_with_its_tab_label() {
        let agents = vec![
            agent("w8:p1", "w8:t1", "w8"),
            non_agent_pane("w8:p4", "w8:t1", "w8"),
            named("w8:p2", "w8:t2", "w8", Some("release-bot")),
            agent("w9:p1", "w9:t1", "w9"),
        ];
        let mut tabs = HashMap::new();
        tabs.insert("w8:t1".to_string(), "Grip Outreach".to_string());
        // w8:t2 has no label, so that row shows its state alone.
        let rows = rows(&agents, Some("w8"), Some("w8:p9"), &tabs);
        assert_eq!(
            rows,
            vec![
                AgentChoice {
                    pane_id: "w8:p1".into(),
                    name: "claude".into(),
                    state: "working".into(),
                    tab: "Grip Outreach".into(),
                },
                AgentChoice {
                    pane_id: "w8:p2".into(),
                    name: "release-bot".into(),
                    state: "working".into(),
                    tab: String::new(),
                },
            ]
        );
    }

    #[test]
    fn picker_rows_exclude_our_own_pane_and_every_non_agent_pane() {
        // A shell and our own pane are not candidates, so neither becomes a row.
        let agents = vec![
            agent("w3:p1", "w3:t1", "w3"),
            non_agent_pane("w3:p4", "w3:t1", "w3"),
            agent("w3:p5", "w3:t1", "w3"),
        ];
        let rows_of = |ws| rows(&agents, ws, Some("w3:p5"), &HashMap::new());
        let picked: Vec<_> = rows_of(Some("w3")).iter().map(|r| r.pane_id.clone()).collect();
        assert_eq!(picked, ["w3:p1"]);
        // No workspace id means no candidates — never every agent on the machine.
        assert!(rows_of(None).is_empty());
    }

    #[test]
    fn an_agent_list_entry_parses_without_any_of_the_picker_fields() {
        // Exactly what herdr 0.7.5 emits: no `name`, no `display_agent`, no `state_labels`.
        let json = r#"{"result":{"agents":[{"agent":"claude","agent_status":"idle","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8"}]}}"#;
        let parsed = parse_agents(json).unwrap();
        assert_eq!(parsed[0].row_name(), "claude");
        assert_eq!(parsed[0].row_state(), "idle");
        // And with `name` explicitly null, as `herdr agent rename --clear` leaves it.
        let cleared = r#"{"result":{"agents":[{"agent":"codex","agent_status":"idle","pane_id":"w8:p2","tab_id":"w8:t1","workspace_id":"w8","name":null}]}}"#;
        assert_eq!(parse_agents(cleared).unwrap()[0].row_name(), "codex");
    }

    #[test]
    fn a_send_is_one_bracketed_paste_with_the_platforms_newlines() {
        // (text, unix bytes, Windows bytes). Windows breaks lines as CRLF, as herdr's own paste
        // does (`export::crlf_line_breaks`).
        let rows = [
            // Issue #41's repro string: sent raw, vim ate the leading `b` and `i`.
            (
                "bit/DESIGN.md:95 note",
                "\x1b[200~bit/DESIGN.md:95 note\x1b[201~",
                "\x1b[200~bit/DESIGN.md:95 note\x1b[201~",
            ),
            (
                "a.rs:2\n+b\nok",
                "\x1b[200~a.rs:2\n+b\nok\x1b[201~",
                "\x1b[200~a.rs:2\r\n+b\r\nok\x1b[201~",
            ),
        ];
        for (text, unix, windows) in rows {
            let want = if cfg!(windows) { windows } else { unix };
            assert_eq!(super::paste_payload(text), want, "{text:?}");
        }
    }

    /// A herdr that accepts the connection and never answers holds the exchange only until its
    /// deadline. Then the exchange ends, and its thread and descriptor with it.
    #[cfg(unix)]
    #[test]
    fn an_unanswered_exchange_ends_at_its_deadline() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let held = listener.accept();
            let _ = released.recv();
            drop(held);
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let deadline = Instant::now() + Duration::from_millis(200);
        std::thread::spawn(move || {
            let _ = tx.send(super::socket::exchange(path.as_os_str(), "{}", deadline));
        });
        let outcome = rx.recv_timeout(Duration::from_secs(5)).expect("the exchange ends");
        let kind = outcome.expect_err("no reply came").kind();
        assert!(matches!(kind, std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        drop(release);
    }

    #[test]
    fn only_a_result_reply_is_a_delivered_send() {
        let ok = "{\"id\":\"reviewr:send\",\"result\":{\"type\":\"ok\"}}\n";
        assert_eq!(super::reply_outcome(ok), Ok(()));
        // herdr's error reply echoes the id and carries the same envelope as a failed CLI call.
        let gone = r#"{"id":"reviewr:send","error":{"code":"pane_not_found","message":"pane w8:p1 not found"}}"#;
        assert_eq!(
            super::reply_outcome(gone),
            Err(HerdrError::Refused(Some("pane_not_found".into())))
        );
        assert_eq!(super::reply_outcome(r#"{"id":"reviewr:send"}"#), Err(HerdrError::Unreadable));
    }

    #[test]
    fn an_embedded_paste_terminator_cannot_end_the_frame_early() {
        // A diff snippet is raw file content and can carry the terminator. The second
        // input splices one together across a removal.
        assert_eq!(super::pasted("a\x1b[201~b"), "\x1b[200~ab\x1b[201~");
        assert_eq!(super::pasted("a\x1b[201\x1b[201~~b"), "\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn a_pane_label_reads_only_our_pane_and_absent_or_empty_is_none() {
        // The live `pane list` entry shape (docs/herdr-api-notes.md): `label` appears only
        // on labeled panes. The label logic stamps the unlabeled and clears only its own.
        let json = r#"{"result":{"panes":[{"pane_id":"w1:p1","label":"build"},{"pane_id":"w1:p2"},{"pane_id":"w1:p3","label":""}]}}"#;
        let list: super::PaneList = super::answer(json).unwrap();
        assert_eq!(list.label("w1:p1"), Some("build"));
        assert_eq!(list.label("w1:p2"), None);
        assert_eq!(list.label("w1:p3"), None, "empty label reads as none");
        assert_eq!(list.label("w9:p9"), None, "unknown pane reads as none");
    }

    #[test]
    fn a_herdr_answer_missing_its_shape_is_unreadable_never_empty() {
        // An error envelope on stdout with exit 0 has no `result`: reading it as an empty
        // listing would make every reviewr pane in the workspace invisible to the actions.
        let envelope = r#"{"error":{"code":"internal","message":"boom"},"id":"cli:request"}"#;
        assert_eq!(super::answer::<super::PaneList>(envelope).err(), Some(HerdrError::Unreadable));
        assert_eq!(super::ProcessInfo::parse(envelope).err(), Some(HerdrError::Unreadable));
        // herdr skips an empty `foreground_processes` when it serializes, so an answer for
        // its pane without the key is a real answer of zero processes.
        let bare = r#"{"result":{"process_info":{"pane_id":"w1:p1","shell_pid":7}}}"#;
        assert_eq!(super::ProcessInfo::parse(bare).unwrap().foreground_processes.len(), 0);
    }

    #[test]
    fn a_failed_call_carries_herdrs_error_code() {
        // herdr writes one JSON envelope to stderr (docs/herdr-api-notes.md). The code is what
        // tells a pane that exited mid-sweep from a herdr that failed.
        let gone = r#"{"error":{"code":"pane_not_found","message":"pane w1:p3 not found"},"id":"cli:request"}"#;
        assert_eq!(super::error_code(gone).as_deref(), Some("pane_not_found"));
        assert!(HerdrError::Refused(super::error_code(gone)).pane_gone());
        // An advisory line before the envelope does not hide it.
        let noisy = format!("warning: something\n{gone}\n");
        assert_eq!(super::error_code(&noisy).as_deref(), Some("pane_not_found"));
        let internal = r#"{"error":{"code":"internal","message":"boom"}}"#;
        assert!(!HerdrError::Refused(super::error_code(internal)).pane_gone());
        assert_eq!(super::error_code("plain words"), None);
    }

    #[test]
    fn a_tab_list_parses_to_labels_and_an_unlabelled_tab_is_dropped() {
        // The documented envelope (docs/herdr-api-notes.md): `label` can be absent.
        let json = r#"{"result":{"tabs":[{"tab_id":"w8:t1","label":"Grip Outreach","number":1,"pane_count":2},{"tab_id":"w8:t2","number":2,"pane_count":1}]}}"#;
        let labels = parse_tab_labels(json).unwrap();
        assert_eq!(labels.get("w8:t1").map(String::as_str), Some("Grip Outreach"));
        assert!(!labels.contains_key("w8:t2"));
        assert!(parse_tab_labels("[]").is_err());
    }

    #[test]
    fn parse_agents_accepts_only_the_documented_envelope() {
        // `cwd` is asserted from the wire on purpose: it is the one field worktree
        // membership rides on, so a renamed key must fail here, not silently in production.
        let wrapped = r#"{"result":{"agents":[{"agent":"claude","agent_status":"working","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/one"}]}}"#;
        assert_eq!(
            parse_agents(wrapped).unwrap(),
            [AgentPane { cwd: Some("/w/one".into()), ..agent("w8:p1", "w8:t1", "w8") }]
        );
        assert!(parse_agents("[]").is_err());
    }

    #[test]
    fn a_state_herdr_adds_names_itself_on_the_row_and_is_unknown_to_tracking() {
        let bare = r#"{"result":{"agents":[{"agent":"claude","agent_status":"compacting","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8"}]}}"#;
        let parsed = parse_agents(bare).unwrap();
        assert_eq!(parsed[0].row_state(), "compacting", "the row shows herdr's own spelling");
        assert_eq!(parsed[0].status(), Status::Unknown, "tracking folds it to unknown");
        // And the spelling is the `state_labels` key, so herdr can label a state reviewr has
        // never heard of.
        let labelled = r#"{"result":{"agents":[{"agent":"claude","agent_status":"compacting","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","state_labels":{"compacting":"Compacting"}}]}}"#;
        assert_eq!(parse_agents(labelled).unwrap()[0].row_state(), "Compacting");
    }
}
