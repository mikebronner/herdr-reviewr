//! Where the event loop's input comes from.
//!
//! On macOS and Linux it is crossterm's own `poll` and `read`, untouched. On Windows crossterm
//! reads the classic console API, where `ConPTY` drops the bracketed-paste markers: a multi-line
//! paste arrives as keystrokes, its first newline submits the comment, and the rest runs as
//! normal-mode keys. There the console is read in VT input mode instead (`windows.rs`), and the
//! bytes parse into the same crossterm events (`vt.rs`). Both go once crossterm reads Windows
//! input that way itself (crossterm PR #1030).

#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod vt;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub(crate) use ratatui::crossterm::event::{poll, read};
#[cfg(windows)]
pub(crate) use windows::{claim, poll, read, release};

/// Claim what the reader needs beyond crossterm's input modes. Nothing on unix.
#[cfg(not(windows))]
pub(crate) fn claim() {}

/// Release what [`claim`] claimed. Nothing on unix.
#[cfg(not(windows))]
pub(crate) fn release() {}
