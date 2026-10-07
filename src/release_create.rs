//! A new GitHub release on `origin`: the draft, its version rules, and its two `gh` requests.

use crate::forge::GhError;
use crate::git::RepoTarget;
use crate::releases::{Release, ReleasesSnapshot};

/// The draft's input field with the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Version,
    Title,
}

/// Where the draft stands; only `Confirm` takes the publish key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Edit,
    /// Everything that will be published is on screen, checked, and waiting for the key.
    Confirm,
    Publishing,
}

/// A release being written, before anything leaves the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseDraft {
    /// Tags this draft's worker results, so a cancelled draft's result lands nowhere.
    pub id: u64,
    pub repository: RepoTarget,
    pub branch: String,
    /// The default branch's head, which the release and its new tag point at.
    pub target: String,
    /// `origin`'s highest version tag, where generated notes start.
    pub previous: Option<String>,
    pub version: String,
    pub title: String,
    /// Whether the title still follows the version, until it is typed in.
    title_follows: bool,
    /// The latest release, whose title the new one copies.
    pattern: Option<Release>,
    existing: Vec<String>,
    pub field: Field,
    pub notes: String,
    pub generating: bool,
    /// The generate request in flight; only its result lands, and an edit drops it.
    pending: Option<u64>,
    requests: u64,
    pub stage: Stage,
    /// Why the last check, generation, or publish failed, as said.
    pub error: Option<String>,
}

/// A request a draft owes a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Generate { id: u64, seq: u64, host: String, args: Vec<String> },
    Publish { id: u64, release: Publish },
}

/// Everything one publish sends, fixed when the draft is confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publish {
    pub repository: RepoTarget,
    pub tag: String,
    pub target: String,
    pub title: String,
    pub notes: String,
}

impl ReleaseDraft {
    /// A draft cutting every unreleased commit, or `None` when nothing waits to be released.
    #[must_use]
    pub fn new(snapshot: &ReleasesSnapshot, id: u64) -> Option<Self> {
        if !snapshot.has_unreleased() || snapshot.head.is_empty() {
            return None;
        }
        let previous = snapshot.versions.first().map(|version| version.tag.clone());
        let version = next_patch(previous.as_deref());
        let pattern = snapshot.releases.first().cloned();
        Some(Self {
            id,
            repository: snapshot.repository.clone(),
            branch: snapshot.branch.clone(),
            target: snapshot.head.clone(),
            previous,
            title: title_for(pattern.as_ref(), &version),
            version,
            title_follows: true,
            pattern,
            existing: snapshot.tags.values().flatten().cloned().collect(),
            field: Field::Version,
            notes: String::new(),
            generating: false,
            pending: None,
            requests: 0,
            stage: Stage::Edit,
            error: None,
        })
    }

    /// Type `ch` into the field with the keyboard.
    pub fn type_char(&mut self, ch: char) {
        self.error = None;
        match self.field {
            Field::Version => {
                self.version.push(ch);
                self.version_changed();
            }
            Field::Title => {
                self.title.push(ch);
                self.title_follows = false;
            }
        }
    }

    /// Delete the field's last character.
    pub fn backspace(&mut self) {
        self.error = None;
        match self.field {
            Field::Version => {
                self.version.pop();
                self.version_changed();
            }
            Field::Title => {
                self.title.pop();
                self.title_follows = false;
            }
        }
    }

    /// A new version retitles a following title and drops notes generated for the old one.
    fn version_changed(&mut self) {
        self.generating = false;
        self.pending = None;
        if self.title_follows {
            self.title = title_for(self.pattern.as_ref(), self.version.trim());
        }
    }

    pub fn switch_field(&mut self) {
        self.field = match self.field {
            Field::Version => Field::Title,
            Field::Title => Field::Version,
        };
    }

    /// The checked tag, or why the version cannot be one.
    pub fn checked_tag(&self) -> Result<String, String> {
        check_version(&self.version, &self.existing)
    }

    /// Ask GitHub for its generated notes, for a version that checks out.
    pub fn generate(&mut self) -> Option<Request> {
        if self.generating {
            self.error = Some("The notes are already generating.".to_string());
            return None;
        }
        let tag = match self.checked_tag() {
            Ok(tag) => tag,
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        self.error = None;
        self.generating = true;
        self.requests += 1;
        self.pending = Some(self.requests);
        Some(Request::Generate {
            id: self.id,
            seq: self.requests,
            host: self.repository.host().to_string(),
            args: generate_args(&self.repository, &tag, &self.target, self.previous.as_deref()),
        })
    }

    /// Land the generated notes this draft still waits for; any other result is stale.
    pub fn generated(&mut self, id: u64, seq: u64, notes: Result<String, String>) {
        if id != self.id || self.pending != Some(seq) {
            return;
        }
        self.pending = None;
        self.generating = false;
        match notes {
            Ok(notes) => self.notes = notes,
            Err(error) => self.error = Some(error),
        }
    }

    /// Show what will be published, once the version checks out.
    pub fn review(&mut self) {
        if self.generating {
            self.error = Some("The notes are still generating.".to_string());
            return;
        }
        match self.checked_tag() {
            Ok(tag) => {
                self.version = tag;
                self.error = None;
                self.stage = Stage::Confirm;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Back from the confirm step to editing.
    pub fn back(&mut self) {
        if self.stage == Stage::Confirm {
            self.stage = Stage::Edit;
        }
    }

    /// The publish, and only from the confirm step: the explicit key is the one way out.
    pub fn confirm(&mut self) -> Option<Request> {
        if self.stage != Stage::Confirm {
            return None;
        }
        self.stage = Stage::Publishing;
        let title =
            if self.title.trim().is_empty() { self.version.clone() } else { self.title.clone() };
        Some(Request::Publish {
            id: self.id,
            release: Publish {
                repository: self.repository.clone(),
                tag: self.version.clone(),
                target: self.target.clone(),
                title,
                notes: self.notes.clone(),
            },
        })
    }

    /// A failed publish goes back to editing with GitHub's own words.
    pub fn failed(&mut self, error: String) {
        self.stage = Stage::Edit;
        self.error = Some(error);
    }
}

use crate::releases::version as version_of;

/// The next patch version after `highest`, spelled in its style; `v0.1.0` with no version yet.
#[must_use]
pub fn next_patch(highest: Option<&str>) -> String {
    let Some((tag, version)) = highest.and_then(|tag| Some((tag, version_of(tag)?))) else {
        return "v0.1.0".to_string();
    };
    let prefix = if tag.trim().starts_with('v') { "v" } else { "" };
    format!("{prefix}{}.{}.{}", version.major, version.minor, version.patch + 1)
}

/// The new release's title in the latest release's pattern, its version swapped in; else the tag.
#[must_use]
pub fn title_for(latest: Option<&Release>, tag: &str) -> String {
    let bare = |tag: &str| tag.strip_prefix('v').unwrap_or(tag).to_string();
    match latest {
        Some(release) if release.name.contains(&release.tag) => {
            release.name.replace(&release.tag, tag)
        }
        Some(release) if release.name.contains(&bare(&release.tag)) => {
            release.name.replace(&bare(&release.tag), &bare(tag))
        }
        _ => tag.to_string(),
    }
}

/// `input` as a tag: a semver version, its `v` optional, no tag on `origin` spelling it already.
pub fn check_version(input: &str, existing: &[String]) -> Result<String, String> {
    let tag = input.trim();
    if tag.is_empty() {
        return Err("Type a version, like v1.2.3.".to_string());
    }
    let Some(version) = version_of(tag) else {
        return Err(format!("{tag} is not a semver version, like v1.2.3."));
    };
    if let Some(taken) = existing.iter().find(|other| version_of(other).as_ref() == Some(&version))
    {
        return Err(format!("{taken} already exists on origin."));
    }
    Ok(tag.to_string())
}

/// `HOST/OWNER/NAME`, the `--repo` spelling that names the host too.
fn repo_flag(repository: &RepoTarget) -> String {
    format!("{}/{}/{}", repository.host(), repository.owner(), repository.name())
}

/// GitHub's own release-notes generation, the call behind its web button. It stores nothing.
#[must_use]
pub fn generate_args(
    repository: &RepoTarget,
    tag: &str,
    target: &str,
    previous: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "api".to_string(),
        "--hostname".to_string(),
        repository.host().to_string(),
        "--method".to_string(),
        "POST".to_string(),
        format!("repos/{}/{}/releases/generate-notes", repository.owner(), repository.name()),
        "-f".to_string(),
        format!("tag_name={tag}"),
        "-f".to_string(),
        format!("target_commitish={target}"),
    ];
    if let Some(previous) = previous {
        args.extend(["-f".to_string(), format!("previous_tag_name={previous}")]);
    }
    args
}

/// The read that proves `tag` is free on `origin`, past the newest tags the list read.
fn tag_ref_args(repository: &RepoTarget, tag: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        "--hostname".to_string(),
        repository.host().to_string(),
        format!("repos/{}/{}/git/ref/tags/{tag}", repository.owner(), repository.name()),
    ]
}

/// The one GitHub write: a release whose tag GitHub cuts at `target`, never a local tag.
#[must_use]
pub fn create_args(release: &Publish) -> Vec<String> {
    vec![
        "release".to_string(),
        "create".to_string(),
        release.tag.clone(),
        format!("--repo={}", repo_flag(&release.repository)),
        format!("--target={}", release.target),
        format!("--title={}", release.title),
        format!("--notes={}", release.notes),
    ]
}

/// Run generate-notes through `run`, a `gh` runner, and return the notes.
pub(crate) fn generate(
    run: &dyn Fn(&[String]) -> Result<String, GhError>,
    args: &[String],
) -> Result<String, String> {
    let out = run(args).map_err(said)?;
    let response: serde_json::Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    response["body"].as_str().map(String::from).ok_or_else(|| "GitHub returned no notes".into())
}

/// Publish through `run`: refuse a tag `origin` already has, then create; the release's URL.
pub(crate) fn publish(
    run: &dyn Fn(&[String]) -> Result<String, GhError>,
    release: &Publish,
) -> Result<String, String> {
    match run(&tag_ref_args(&release.repository, &release.tag)) {
        Ok(_) => return Err(format!("{} already exists on origin.", release.tag)),
        Err(GhError::Other(message) | GhError::NotFound(message))
            if crate::forge::reports_status(&message.to_lowercase(), 404) => {}
        Err(error) => return Err(said(error)),
    }
    run(&create_args(release)).map(|url| url.trim().to_string()).map_err(said)
}

/// What `gh` said, as the screen shows it.
fn said(error: GhError) -> String {
    match error {
        GhError::NoGh => "GitHub CLI not found. Install `gh`.".to_string(),
        GhError::NotAuthed(host) => format!("Not signed in to {host}."),
        GhError::LocalGit(message) | GhError::NotFound(message) | GhError::Other(message) => {
            message
        }
    }
}

/// The real runner: `gh` in `repo` against `host`, off the frame loop by its caller.
pub(crate) fn gh_runner(
    repo: std::path::PathBuf,
    host: String,
) -> impl Fn(&[String]) -> Result<String, GhError> {
    move |args: &[String]| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::forge::gh(&repo, &host, &args, &std::sync::atomic::AtomicBool::new(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn target() -> RepoTarget {
        RepoTarget::new("github.com", "me", "tool").unwrap()
    }

    fn release(tag: &str, name: &str) -> Release {
        Release {
            tag: tag.into(),
            name: name.into(),
            notes: String::new(),
            draft: false,
            prerelease: false,
        }
    }

    fn sample() -> Publish {
        Publish {
            repository: target(),
            tag: "v1.2.4".into(),
            target: "abc123".into(),
            title: "tool 1.2.4".into(),
            notes: "## What's Changed\n* a fix".into(),
        }
    }

    #[test]
    fn one_leading_v_is_a_tag_style_and_two_are_not_a_version() {
        assert_eq!(version_of("v1.2.3"), crate::releases::version("v1.2.3"));
        assert!(version_of("vv1.2.3").is_none());
        assert!(check_version("vv1.2.3", &[]).is_err());
    }

    #[test]
    fn the_next_patch_keeps_the_tag_style() {
        assert_eq!(next_patch(Some("v0.46.0")), "v0.46.1");
        assert_eq!(next_patch(Some("1.9.9")), "1.9.10");
        assert_eq!(next_patch(Some("v2.0.0-rc.1")), "v2.0.1");
        assert_eq!(next_patch(None), "v0.1.0");
        assert_eq!(next_patch(Some("nightly")), "v0.1.0");
    }

    #[test]
    fn the_title_follows_the_latest_release_pattern_else_the_tag() {
        assert_eq!(
            title_for(Some(&release("v0.46.0", "herdr-reviewr 0.46.0")), "v0.46.1"),
            "herdr-reviewr 0.46.1"
        );
        assert_eq!(title_for(Some(&release("v2", "Release v2")), "v3"), "Release v3");
        assert_eq!(title_for(Some(&release("v2.0.0", "Big one")), "v2.0.1"), "v2.0.1");
        assert_eq!(title_for(None, "v0.1.0"), "v0.1.0");
    }

    #[test]
    fn a_version_must_be_semver_and_new_on_origin() {
        let existing = vec!["v0.46.0".to_string(), "nightly".to_string()];
        assert_eq!(check_version(" v0.46.1 ", &existing), Ok("v0.46.1".into()));
        assert_eq!(check_version("0.47.0", &existing), Ok("0.47.0".into()));
        assert!(check_version("", &existing).unwrap_err().contains("Type a version"));
        assert!(check_version("v0.46", &existing).unwrap_err().contains("not a semver version"));
        assert!(check_version("nightly", &existing).unwrap_err().contains("not a semver"));
        assert_eq!(
            check_version("v0.46.0", &existing),
            Err("v0.46.0 already exists on origin.".into())
        );
        assert_eq!(
            check_version("0.46.0", &existing),
            Err("v0.46.0 already exists on origin.".into()),
            "the same version in the other spelling is taken too"
        );
    }

    #[test]
    fn generate_asks_github_for_that_tag_target_and_previous_version() {
        assert_eq!(
            generate_args(&target(), "v1.2.4", "abc123", Some("v1.2.3")),
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/me/tool/releases/generate-notes",
                "-f",
                "tag_name=v1.2.4",
                "-f",
                "target_commitish=abc123",
                "-f",
                "previous_tag_name=v1.2.3",
            ]
        );
        let first = generate_args(&target(), "v0.1.0", "abc123", None);
        assert!(!first.iter().any(|arg| arg.starts_with("previous_tag_name")), "{first:?}");
        let body = r###"{"name": "v1.2.4", "body": "## What's Changed"}"###;
        assert_eq!(generate(&|_| Ok(body.into()), &first), Ok("## What's Changed".into()));
        assert_eq!(
            generate(&|_| Err(GhError::Other("HTTP 422".into())), &first),
            Err("HTTP 422".into())
        );
    }

    #[test]
    fn publish_creates_the_release_with_an_explicit_target_and_never_a_local_tag() {
        assert_eq!(
            create_args(&sample()),
            [
                "release",
                "create",
                "v1.2.4",
                "--repo=github.com/me/tool",
                "--target=abc123",
                "--title=tool 1.2.4",
                "--notes=## What's Changed\n* a fix",
            ]
        );
        let calls = RefCell::new(Vec::new());
        let run = |args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            if args[0] == "api" {
                Err(GhError::Other("gh: Not Found (HTTP 404)".into()))
            } else {
                Ok("https://github.com/me/tool/releases/tag/v1.2.4\n".into())
            }
        };
        assert_eq!(
            publish(&run, &sample()),
            Ok("https://github.com/me/tool/releases/tag/v1.2.4".into())
        );
        let calls = calls.into_inner();
        assert_eq!(
            calls[0],
            ["api", "--hostname", "github.com", "repos/me/tool/git/ref/tags/v1.2.4"]
        );
        assert_eq!(calls[1], create_args(&sample()));
        assert_eq!(calls.len(), 2, "one read, then the release; GitHub cuts its tag");
    }

    #[test]
    fn publish_refuses_a_taken_tag_and_reports_a_failure_without_creating() {
        let calls = RefCell::new(0);
        let taken = |args: &[String]| {
            *calls.borrow_mut() += 1;
            assert_eq!(args[0], "api", "only the read runs");
            Ok("{\"ref\": \"refs/tags/v1.2.4\"}".into())
        };
        assert_eq!(publish(&taken, &sample()), Err("v1.2.4 already exists on origin.".into()));
        assert_eq!(*calls.borrow(), 1);
        let unauthed = |args: &[String]| {
            assert_eq!(args[0], "api", "an unreadable tag never creates");
            Err(GhError::NotAuthed("github.com".into()))
        };
        assert_eq!(publish(&unauthed, &sample()), Err("Not signed in to github.com.".into()));
        let refused = |args: &[String]| {
            if args[0] == "api" {
                Err(GhError::Other("HTTP 404".into()))
            } else {
                Err(GhError::Other("HTTP 403: Resource not accessible by integration".into()))
            }
        };
        assert_eq!(
            publish(&refused, &sample()),
            Err("HTTP 403: Resource not accessible by integration".into()),
            "gh's own words"
        );
    }

    fn snapshot(root: bool) -> ReleasesSnapshot {
        let mut tags = std::collections::HashMap::new();
        tags.insert("c1".to_string(), vec!["v0.46.0".to_string()]);
        ReleasesSnapshot {
            repository: target(),
            branch: "main".into(),
            head: "c2".into(),
            root: if root {
                vec![crate::releases::ReleaseCommit { oid: "c2".into(), ..Default::default() }]
            } else {
                vec![]
            },
            versions: vec![crate::releases::VersionTag {
                tag: "v0.46.0".into(),
                oid: "c1".into(),
                commit: crate::releases::ReleaseCommit::default(),
            }],
            tags,
            releases: vec![release("v0.46.0", "tool 0.46.0")],
            upstream: None,
        }
    }

    #[test]
    fn a_draft_cuts_the_unreleased_head_prefilled_and_publishes_only_from_confirm() {
        assert_eq!(ReleaseDraft::new(&snapshot(false), 1), None, "nothing waits to be released");
        let mut draft = ReleaseDraft::new(&snapshot(true), 7).unwrap();
        assert_eq!((draft.version.as_str(), draft.title.as_str()), ("v0.46.1", "tool 0.46.1"));
        assert_eq!((draft.target.as_str(), draft.previous.as_deref()), ("c2", Some("v0.46.0")));
        assert_eq!(draft.confirm(), None, "editing never publishes");
        // The title follows the version until it is typed in.
        draft.backspace();
        draft.type_char('2');
        assert_eq!(draft.title, "tool 0.46.2");
        draft.switch_field();
        draft.type_char('!');
        draft.switch_field();
        draft.backspace();
        draft.type_char('3');
        assert_eq!(draft.title, "tool 0.46.2!", "a typed title stays");
        // A taken version stops at the review, with the reason.
        draft.version = "v0.46.0".into();
        draft.review();
        assert_eq!(draft.stage, Stage::Edit);
        assert_eq!(draft.error.as_deref(), Some("v0.46.0 already exists on origin."));
        assert_eq!(draft.generate(), None, "nor does it generate");
        draft.version = "v0.46.3".into();
        let Some(Request::Generate { id: 7, seq, host, args }) = draft.generate() else {
            panic!("generate")
        };
        assert!(args.contains(&"tag_name=v0.46.3".to_string()) && draft.generating);
        assert_eq!(host, "github.com");
        draft.review();
        assert_eq!(draft.stage, Stage::Edit, "no review while the notes generate");
        assert_eq!(draft.generate(), None, "one generation at a time");
        draft.generated(6, seq, Ok("stale".into()));
        assert_eq!(draft.notes, "", "another draft's notes never land");
        draft.generated(7, seq + 1, Ok("stale".into()));
        assert_eq!(draft.notes, "", "another request's notes never land");
        draft.generated(7, seq, Ok("## Notes".into()));
        assert_eq!((draft.notes.as_str(), draft.generating), ("## Notes", false));
        // Notes asked for one version never land on another.
        let Some(Request::Generate { seq: old, .. }) = draft.generate() else { panic!("generate") };
        draft.backspace();
        draft.type_char('3');
        assert!(!draft.generating, "a version edit drops the request in flight");
        draft.generated(7, old, Ok("for the old version".into()));
        assert_eq!(draft.notes, "## Notes", "a stale request's notes never land");
        draft.review();
        assert_eq!(draft.stage, Stage::Confirm);
        draft.back();
        assert_eq!(draft.stage, Stage::Edit);
        assert_eq!(draft.confirm(), None);
        draft.review();
        let Some(Request::Publish { id: 7, release }) = draft.confirm() else { panic!("publish") };
        assert_eq!(
            release,
            Publish {
                repository: target(),
                tag: "v0.46.3".into(),
                target: "c2".into(),
                title: "tool 0.46.2!".into(),
                notes: "## Notes".into(),
            }
        );
        assert_eq!(draft.confirm(), None, "one confirm, one publish");
        draft.failed("HTTP 422".into());
        assert_eq!((draft.stage.clone(), draft.error.as_deref()), (Stage::Edit, Some("HTTP 422")));
    }
}
