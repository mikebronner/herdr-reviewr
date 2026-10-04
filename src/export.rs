//! Formatting comments and exporting them to the agent or clipboard.
//!
//! A comment becomes a block of `location`, the
//! diff snippet, then the text. Export is consume-on-success: the caller removes
//! a comment only after `export` returns `Ok`.

use anyhow::Result;

use crate::herdr;
use crate::model::Comment;

/// One comment as its export block: location, snippet, then text.
pub fn format_comment(comment: &Comment) -> String {
    format!("{}\n{}\n{}", comment.location(), comment.lines, normalize_text(&comment.text))
}

/// Comment text for export: drop `\r`, trim trailing space per line, and drop blank
/// lines so a multi-line comment can never introduce the blank-line block separator.
fn normalize_text(text: &str) -> String {
    text.replace('\r', "")
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` with Windows' line breaks: each `\n` becomes CRLF, a CRLF already there stays one,
/// and a lone CR, which breaks no line, passes through. The clipboard and herdr's paste both
/// break lines this way on Windows.
pub(crate) fn crlf_line_breaks(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Many comments, sorted by file then start line, one blank line between blocks.
pub fn format_all(comments: &[&Comment]) -> String {
    let mut sorted = comments.to_vec();
    sorted.sort_by(|a, b| a.file.cmp(&b.file).then(a.start.cmp(&b.start)));
    sorted.iter().map(|c| format_comment(c)).collect::<Vec<_>>().join("\n\n")
}

/// A destination comments can be exported to. Export succeeds or errors as a whole.
pub trait ExportTarget {
    fn export(&self, text: &str) -> Result<()>;
    fn label(&self) -> &'static str;
    /// Destination-specific confirmation shown after a successful export.
    fn success_message(&self, count: usize) -> String;
    /// Destination-specific line shown after a failed one, given the error [`Self::export`]
    /// returned. It is the whole status, so it is one short sentence a reviewer can read, never
    /// the underlying error. The cause goes to the log.
    fn failure_message(&self, error: &anyhow::Error) -> String;
}

pub(crate) fn counted_comments(count: usize) -> String {
    let noun = if count == 1 { "comment" } else { "comments" };
    format!("{count} {noun}")
}

/// The system clipboard: the first clipboard tool on `PATH`, or on Windows the Win32 clipboard
/// itself.
#[derive(Debug)]
pub struct Clipboard;

impl ExportTarget for Clipboard {
    fn label(&self) -> &'static str {
        "clipboard"
    }

    fn success_message(&self, count: usize) -> String {
        format!("copied {}", counted_comments(count))
    }

    fn failure_message(&self, error: &anyhow::Error) -> String {
        match clipboard::remedy(error) {
            Some(remedy) => format!("copy failed: {remedy}"),
            None => "copy failed".to_string(),
        }
    }

    fn export(&self, text: &str) -> Result<()> {
        clipboard::write(text)
    }
}

/// macOS and Linux copy through a clipboard tool.
#[cfg(not(windows))]
mod clipboard {
    use std::io::Write;
    use std::process::Stdio;

    use anyhow::{Context, Result, bail};

    /// A clipboard tool and the args that make it read stdin into the system clipboard. Tried
    /// in order — the first one present on `PATH` wins. macOS ships `pbcopy`; Linux needs one
    /// of these installed (Wayland `wl-copy`, or X11 `xclip`/`xsel`). OSC 52 is roadmap.
    pub(super) const TOOLS: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    /// No clipboard tool on `PATH`: the one copy failure the reviewer can fix, so its line says
    /// how.
    #[derive(Debug)]
    pub(super) struct NoTool;

    impl std::fmt::Display for NoTool {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "no clipboard tool found (wl-clipboard, xclip, or xsel)")
        }
    }

    impl std::error::Error for NoTool {}

    pub(super) fn remedy(error: &anyhow::Error) -> Option<&'static str> {
        error.is::<NoTool>().then_some("install wl-clipboard, xclip, or xsel")
    }

    /// Pipe `text` into the first tool on `PATH`, succeeding only when it exits clean.
    pub(super) fn write(text: &str) -> Result<()> {
        let (cmd, args) = select_tool(TOOLS, crate::proc::on_path).ok_or(NoTool)?;
        let mut child = crate::proc::command(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {cmd}"))?;
        child
            .stdin
            .as_mut()
            .with_context(|| format!("{cmd} stdin unavailable"))?
            .write_all(text.as_bytes())
            .with_context(|| format!("writing to {cmd}"))?;
        if !child.wait().with_context(|| format!("waiting for {cmd}"))?.success() {
            bail!("{cmd} exited non-zero");
        }
        Ok(())
    }

    /// The first clipboard tool the `present` predicate accepts, preserving list order.
    pub(super) fn select_tool(
        tools: &'static [(&'static str, &'static [&'static str])],
        present: impl Fn(&str) -> bool,
    ) -> Option<(&'static str, &'static [&'static str])> {
        tools.iter().copied().find(|(cmd, _)| present(cmd))
    }
}

/// Windows writes the Win32 clipboard directly, as Unicode text. No tool is needed, and
/// `clip.exe` would read the text in the console's code page and mangle anything outside ASCII.
#[cfg(windows)]
mod clipboard {
    use anyhow::{Context, Result};

    /// Nothing to install, so no failure here has a remedy to name.
    pub(super) fn remedy(_error: &anyhow::Error) -> Option<&'static str> {
        None
    }

    /// The text outlives the handle: Windows keeps clipboard data after its writer lets go.
    /// Line breaks go in as CRLF, the clipboard's own convention, so an edit control that
    /// splits only on CRLF still shows the review's lines.
    pub(super) fn write(text: &str) -> Result<()> {
        let text = super::crlf_line_breaks(text);
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text))
            .context("writing the Windows clipboard")
    }
}

/// One chosen agent pane: fill its input with one `pane.send_text` request over herdr's socket,
/// then focus it.
///
/// The pane is decided before the export runs, by the sole-agent path or by the picker, and
/// nothing re-resolves it here. A pane that closed in between fails the send and keeps every
/// comment.
#[derive(Clone, Debug)]
pub struct Agent {
    pub pane: String,
    pub name: String,
}

impl ExportTarget for Agent {
    fn label(&self) -> &'static str {
        "agent"
    }

    /// Names the agent it addressed. The send is irreversible and consumes the whole set, so
    /// this line is the reviewer's only record of where the review went.
    fn success_message(&self, count: usize) -> String {
        format!("sent {} to {}", counted_comments(count), self.name)
    }

    /// herdr ran and refused the paste: the pane closed after it was resolved. herdr's own
    /// wording is a JSON envelope around a pane id, so the reviewer gets a sentence and the
    /// payload goes to the log. A [`herdr::Refusal`], a herdr that never answered included,
    /// never reaches here: the app words it.
    fn failure_message(&self, _error: &anyhow::Error) -> String {
        format!("{} closed", self.name)
    }

    /// An agent at a prompt refuses the send ([`herdr::send_text`] reads its state at the moment
    /// of sending), because the picker's rows can be minutes old.
    fn export(&self, text: &str) -> Result<()> {
        herdr::send_text(&self.pane, text)?;
        // Focus is a convenience once the text is delivered; a focus failure must NOT fail the
        // export, or the comments stay unconsumed and the next Send duplicates the whole review.
        let _ = herdr::focus(&self.pane);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Agent, Clipboard, ExportTarget, crlf_line_breaks, format_all, format_comment};
    use crate::model::{Comment, Side};

    #[test]
    fn windows_line_breaks_are_crlf_and_never_doubled() {
        let rows = [
            ("a.rs:2\n+b\nok", "a.rs:2\r\n+b\r\nok"),
            ("a\n\nb\n", "a\r\n\r\nb\r\n"),
            ("a\r\nb", "a\r\nb"),
            ("a\rb", "a\rb"),
        ];
        for (text, want) in rows {
            assert_eq!(crlf_line_breaks(text), want, "{text:?}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn clipboard_tool_selection_prefers_list_order_and_can_be_empty() {
        use super::clipboard::{TOOLS, select_tool};
        // None present -> no tool (the caller surfaces the "install one" error).
        assert!(select_tool(TOOLS, |_| false).is_none());
        // Only an X11 tool present -> it's chosen, with its selection args.
        assert_eq!(
            select_tool(TOOLS, |c| c == "xclip"),
            Some(("xclip", &["-selection", "clipboard"][..]))
        );
        // When several are present, earlier in the list wins (pbcopy over xclip).
        assert_eq!(
            select_tool(TOOLS, |c| c == "pbcopy" || c == "xclip").map(|(cmd, _)| cmd),
            Some("pbcopy")
        );
    }

    /// Windows needs no tool on `PATH`, and the text lands as Unicode with CRLF line breaks:
    /// `clip.exe` would mangle everything past ASCII. Whatever the clipboard held before is put
    /// back.
    #[cfg(windows)]
    #[test]
    fn a_windows_copy_lands_on_the_clipboard_with_non_ascii_intact() {
        let before = arboard::Clipboard::new().and_then(|mut c| c.get_text()).ok();
        Clipboard.export("src/café.rs:3\n+let 日本 = 1;\nnaming 👍").unwrap();
        let copied = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap();
        if let Some(before) = before {
            let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(before));
        }
        assert_eq!(copied, "src/café.rs:3\r\n+let 日本 = 1;\r\nnaming 👍");
    }

    #[test]
    fn export_confirmations_name_the_actual_result_and_pluralize_comments() {
        // The agent line names the pane it addressed, so a mis-send is visible the moment it
        // lands.
        let agent = Agent { pane: "w8:p1".into(), name: "release-bot".into() };
        assert_eq!(agent.success_message(1), "sent 1 comment to release-bot");
        assert_eq!(agent.success_message(2), "sent 2 comments to release-bot");
        assert_eq!(Clipboard.success_message(1), "copied 1 comment");
        assert_eq!(Clipboard.success_message(2), "copied 2 comments");
    }

    #[test]
    fn a_failed_send_or_copy_says_what_to_do() {
        let agent = Agent { pane: "w8:p1".into(), name: "release-bot".into() };
        assert_eq!(agent.failure_message(&anyhow::anyhow!("herdr refused")), "release-bot closed");
        #[cfg(not(windows))]
        {
            let missing = anyhow::Error::from(super::clipboard::NoTool);
            assert_eq!(
                Clipboard.failure_message(&missing),
                "copy failed: install wl-clipboard, xclip, or xsel"
            );
            assert_eq!(
                Clipboard.failure_message(&anyhow::anyhow!("pbcopy exited non-zero")),
                "copy failed"
            );
        }
        // Windows has no tool to install, so a failed write never points at one.
        #[cfg(windows)]
        {
            let busy = anyhow::Error::from(arboard::Error::ClipboardOccupied)
                .context("writing the Windows clipboard");
            assert_eq!(Clipboard.failure_message(&busy), "copy failed");
        }
    }

    fn comment(file: &str, side: Side, start: u32, end: u32, lines: &str, text: &str) -> Comment {
        Comment {
            file: file.into(),
            side,
            start,
            end,
            lines: lines.into(),
            text: text.into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        }
    }

    #[test]
    fn block_is_location_snippet_text() {
        let c = comment(
            "extruct/core/llm_registry.py",
            Side::New,
            40,
            41,
            "-from .z import w\n+from .x import y",
            "this import path looks wrong",
        );
        assert_eq!(
            format_comment(&c),
            "extruct/core/llm_registry.py:40-41\n-from .z import w\n+from .x import y\nthis import path looks wrong"
        );
    }

    #[test]
    fn removed_side_marks_the_header() {
        let c = comment("a.rs", Side::Old, 38, 38, "-    cleanup()", "still needed");
        assert_eq!(format_comment(&c), "a.rs:38 (removed)\n-    cleanup()\nstill needed");
    }

    #[test]
    fn multiline_text_keeps_breaks_but_drops_blank_lines() {
        let c = comment("a.rs", Side::New, 1, 1, "+x", "first line\n\n  \nsecond line\n");
        assert_eq!(format_comment(&c), "a.rs:1\n+x\nfirst line\nsecond line");
    }

    #[test]
    fn all_sorts_by_file_then_start_with_blank_separator() {
        let b = comment("b.rs", Side::New, 5, 5, "+x", "two");
        let a2 = comment("a.rs", Side::New, 20, 20, "+y", "later");
        let a1 = comment("a.rs", Side::New, 3, 3, "+z", "earlier");
        let out = format_all(&[&b, &a2, &a1]);
        assert_eq!(out, "a.rs:3\n+z\nearlier\n\na.rs:20\n+y\nlater\n\nb.rs:5\n+x\ntwo");
    }
}
