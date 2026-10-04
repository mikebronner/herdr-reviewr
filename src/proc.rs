//! Small helpers for locating and naming external command-line tools.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;

/// Usual host bin dirs a stripped pane PATH may omit. Unix only: on Windows these names
/// resolve to directories like `C:\\usr\\bin` on the current drive, which any user can create.
#[cfg(unix)]
const COMMON_BINS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
#[cfg(not(unix))]
const COMMON_BINS: &[&str] = &[];

fn host_path() -> OsString {
    prepended_path(env::var_os("PATH").as_deref())
}

fn common_bins() -> impl Iterator<Item = PathBuf> {
    COMMON_BINS.iter().map(PathBuf::from)
}

/// The inherited PATH's entries. A set-but-empty PATH is the same as none: split, its one
/// empty entry would put the reviewed repository's own working directory ahead of every real
/// bin dir.
fn inherited_dirs(inherited: Option<&OsStr>) -> Vec<PathBuf> {
    inherited.filter(|p| !p.is_empty()).map(|p| env::split_paths(p).collect()).unwrap_or_default()
}

/// Join PATH entries with the platform's separator. Every entry came out of `split_paths` or
/// `COMMON_BINS`, so none holds a separator and the join cannot fail. Were it to, the inherited
/// PATH passes through untouched rather than becoming an empty one.
fn joined(dirs: impl Iterator<Item = PathBuf>, inherited: Option<&OsStr>) -> OsString {
    env::join_paths(dirs).unwrap_or_else(|_| inherited.map(OsStr::to_os_string).unwrap_or_default())
}

fn prepended_path(inherited: Option<&OsStr>) -> OsString {
    joined(common_bins().chain(inherited_dirs(inherited)), inherited)
}

fn appended_path(inherited: Option<&OsStr>) -> OsString {
    joined(inherited_dirs(inherited).into_iter().chain(common_bins()), inherited)
}

/// Resolve `name` against `path` the way a shell would: an executable file, with PATHEXT on
/// Windows, and the current directory only for a name that is itself a path.
fn resolve_on(path: &OsStr, name: &OsStr) -> Option<PathBuf> {
    let cwd = env::current_dir().unwrap_or_default();
    which::which_in(name, Some(path), cwd).ok()
}

/// Resolve `program` on the host PATH — the common host bins first, the inherited PATH after —
/// and give the child that same PATH.
///
/// For the tools reviewr runs for itself. [`user_command`] is the other way round, for the
/// reviewer's own.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let path = host_path();
    let mut cmd = resolve_on(&path, program).map_or_else(|| Command::new(program), Command::new);
    cmd.env("PATH", path);
    cmd
}

/// Resolve `program` the way the reviewer's own shell would: their `PATH` first, the common
/// host bins only as a fallback. The child is given that same PATH. `None` when the name
/// resolves to nothing, so a caller can say so before it acts.
///
/// The opposite order from [`command`], and deliberately. `git` and the forge CLIs are the
/// host's tools, so a stripped pane PATH must not hide them. The editor is the reviewer's own,
/// so a version-managed shim on their `PATH` has to win over a stale copy in a common bin, and
/// so must every tool the editor goes on to launch — its language servers, its formatters, its
/// runtime.
pub(crate) fn user_command(program: impl AsRef<OsStr>) -> Option<Command> {
    let program = program.as_ref();
    let path = appended_path(env::var_os("PATH").as_deref());
    let mut cmd = Command::new(resolve_on(&path, program)?);
    cmd.env("PATH", path);
    Some(cmd)
}

/// A program's name from its path: the base name after the last `/` or `\`, a trailing Windows
/// program extension (`.exe`, `.cmd`, `.bat`) dropped in any case. Both separators end a
/// directory on every OS: a Windows path may spell either, and no program has a backslash in its
/// name. The case stays as spelled, for each caller to compare as it needs.
pub(crate) fn program_name(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rsplit_once('.') {
        Some((stem, ext)) if ["exe", "cmd", "bat"].iter().any(|e| ext.eq_ignore_ascii_case(e)) => {
            stem
        }
        _ => base,
    }
}

/// Whether `name` resolves to an executable on the host PATH. Shared by the clipboard probe
/// (`export.rs`) and the URL-opener probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    resolve_on(&host_path(), OsStr::new(name)).is_some()
}

#[cfg(test)]
mod tests {
    use super::{COMMON_BINS, appended_path, prepended_path, program_name, resolve_on};
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    fn path_of(dirs: &[&str]) -> OsString {
        env::join_paths(dirs).unwrap()
    }

    fn common() -> Vec<PathBuf> {
        COMMON_BINS.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn prepended_path_puts_the_common_bins_in_front_of_the_inherited_path() {
        let got = prepended_path(Some(&path_of(&["inherited-a", "inherited-b"])));
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = common();
        expected.extend([PathBuf::from("inherited-a"), PathBuf::from("inherited-b")]);
        assert_eq!(parts, expected);
    }

    #[test]
    fn prepended_path_keeps_the_common_bins_when_nothing_is_inherited() {
        let got = prepended_path(None);
        let parts: Vec<PathBuf> =
            env::split_paths(&got).filter(|p| !p.as_os_str().is_empty()).collect();
        assert_eq!(parts, common());
    }

    #[test]
    fn appended_path_leaves_the_reviewers_own_entries_in_front() {
        // The editor's own tools have to resolve the way its shell would resolve them, so a
        // version-managed shim wins and the common bins only backstop a stripped PATH.
        let got = appended_path(Some(&path_of(&["mise-shims", "system-bin"])));
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = vec![PathBuf::from("mise-shims"), PathBuf::from("system-bin")];
        expected.extend(common());
        assert_eq!(parts, expected);

        // A set-but-empty PATH is the same as none. Joined instead, its empty entry would put
        // the reviewed repository's own working directory ahead of every real bin dir.
        assert_eq!(appended_path(Some(OsStr::new(""))), appended_path(None));
    }

    #[test]
    fn a_program_name_drops_the_directory_on_either_separator_and_a_windows_extension() {
        let rows = [
            ("target/debug/herdr-reviewr", "herdr-reviewr"),
            (r"C:\Users\me\plugin\bin\herdr-reviewr.exe", "herdr-reviewr"),
            (r"C:\plugin\bin\herdr-reviewr.EXE", "herdr-reviewr"),
            (r"C:\Program Files\Microsoft VS Code\Code.exe", "Code"),
            (r"C:\Users\me\AppData\Roaming\npm\code.CMD", "code"),
            ("C:/tools/edit.bat", "edit"),
            ("herdr-reviewr", "herdr-reviewr"),
            ("/usr/bin/herdr-reviewr-helper", "herdr-reviewr-helper"),
            ("/usr/bin/notepad++", "notepad++"),
            ("/opt/app.d/run.sh", "run.sh"),
            ("é.exe", "é"),
            ("exe", "exe"),
        ];
        for (path, want) in rows {
            assert_eq!(program_name(path), want, "{path:?}");
        }
    }

    /// An executable named `name` in `dir`, spelled the way the platform spells programs.
    #[cfg(unix)]
    fn program(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join(name);
        std::fs::write(&bin, []).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(windows)]
    fn program(dir: &Path, name: &str) -> PathBuf {
        let bin = dir.join(format!("{name}.exe"));
        std::fs::write(&bin, []).unwrap();
        bin
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
    }

    #[test]
    fn resolve_on_finds_a_bare_name_in_a_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = program(dir.path(), "gh");
        let path = env::join_paths([dir.path()]).unwrap();
        let found = resolve_on(&path, OsStr::new("gh")).expect("gh resolves");
        assert!(same_file(&found, &bin), "{found:?} is not {bin:?}");
        assert!(resolve_on(&path, OsStr::new("missing")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_the_executable_bit_is_not_a_program() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes"), []).unwrap();
        let path = env::join_paths([dir.path()]).unwrap();
        assert!(resolve_on(&path, OsStr::new("notes")).is_none());
    }

    /// `az` and `code` ship as batch shims on Windows: a bare name must reach them through
    /// PATHEXT, which `std::process::Command` alone never tries.
    #[cfg(windows)]
    #[test]
    fn a_batch_shim_resolves_through_pathext() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("az.cmd");
        std::fs::write(&shim, "@echo off\r\n").unwrap();
        let path = env::join_paths([dir.path()]).unwrap();
        let found = resolve_on(&path, OsStr::new("az")).expect("az resolves");
        assert!(same_file(&found, &shim), "{found:?} is not {shim:?}");
    }
}
