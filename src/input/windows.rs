//! The Windows console reader: VT input mode, read as UTF-16 key records.
//!
//! With `ENABLE_VIRTUAL_TERMINAL_INPUT` set, the console passes the terminal's input through as
//! the bytes it sent, bracketed-paste markers included, which the classic console API strips.
//! Measured in herdr 0.9.3's `ConPTY` on Windows 11: a paste arrives whole between `ESC[200~` and
//! `ESC[201~`, and non-ASCII text arrives as correct UTF-16 (surrogate pairs for an emoji), so
//! `ReadConsoleInputW` needs no code page change.
//!
//! Two inputs go beyond crossterm's own claims, and both are written as escape sequences because
//! crossterm uses the console API for them on Windows. Mouse tracking: crossterm's mouse capture
//! only sets a console mode, and VT input needs the terminal to report the mouse in SGR form,
//! the way it does on unix. The kitty keyboard protocol's disambiguate flag: VT input encodes
//! Shift+Enter as a bare CR, the same byte as Enter, and the flag keeps the two apart, so
//! Shift+Enter still inserts a newline. A terminal without the protocol ignores the push.

use std::io::{self, Write};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossterm_winapi::{Console, ConsoleMode, Handle, InputRecord};
use ratatui::crossterm::Command;
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

use super::vt::VtInput;

const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// The open console and the parser state between reads. `None` while the terminal is released,
/// so an editor's leftovers never parse as the review's input.
static READER: Mutex<Option<Reader>> = Mutex::new(None);

struct Reader {
    handle: Handle,
    console: Console,
    vt: VtInput,
}

/// Switch the console to VT input and ask the terminal for SGR mouse reports and disambiguated
/// keys.
///
/// Runs after crossterm's mouse capture, which overwrites the whole console input mode, so the
/// VT flag lands on top of it. `claim_terminal` reruns it after an editor hands the pane back.
pub(crate) fn claim() {
    set_vt_input(true);
    let mut sequence = String::new();
    let _ = EnableMouseCapture.write_ansi(&mut sequence);
    let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
    let _ = PushKeyboardEnhancementFlags(flags).write_ansi(&mut sequence);
    write_out(&sequence);
}

/// Undo [`claim`], before crossterm's mouse release restores the console mode it found.
pub(crate) fn release() {
    let mut sequence = String::new();
    let _ = PopKeyboardEnhancementFlags.write_ansi(&mut sequence);
    let _ = DisableMouseCapture.write_ansi(&mut sequence);
    write_out(&sequence);
    set_vt_input(false);
    *READER.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// Whether an event is ready within `timeout`, as `crossterm::event::poll` answers it.
pub(crate) fn poll(timeout: Duration) -> io::Result<bool> {
    with_reader(|reader| reader.poll(Some(Instant::now() + timeout)))
}

/// The next event, blocking until there is one, as `crossterm::event::read` answers it.
pub(crate) fn read() -> io::Result<Event> {
    with_reader(|reader| {
        loop {
            if let Some(event) = reader.vt.pop() {
                return Ok(event);
            }
            reader.poll(None)?;
        }
    })
}

fn with_reader<T>(f: impl FnOnce(&mut Reader) -> io::Result<T>) -> io::Result<T> {
    let mut guard = READER.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_none() {
        let handle = Handle::current_in_handle()?;
        let console = Console::from(handle.clone());
        *guard = Some(Reader { handle, console, vt: VtInput::default() });
    }
    f(guard.as_mut().expect("opened above"))
}

impl Reader {
    /// Read the console until an event is parsed or `deadline` passes.
    ///
    /// The console is checked at least once, so a zero timeout still sees input already queued.
    fn poll(&mut self, deadline: Option<Instant>) -> io::Result<bool> {
        loop {
            let now = Instant::now();
            self.vt.expire(now);
            if self.vt.has_events() {
                return Ok(true);
            }
            // An open paste's bound wakes the wait too, so it closes on time.
            let wake = deadline.into_iter().chain(self.vt.paste_deadline()).min();
            if wait_for_input(&self.handle, wake.map(|wake| wake.saturating_duration_since(now)))? {
                self.read_records()?;
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                self.vt.expire(Instant::now());
                return Ok(self.vt.has_events());
            }
        }
    }

    fn read_records(&mut self) -> io::Result<()> {
        for record in self.console.read_console_input()? {
            match record {
                // A zero unit is a key the terminal sent no byte for, such as a bare modifier.
                InputRecord::KeyEvent(key) if key.key_down && key.u_char != 0 => {
                    self.vt.feed(key.u_char);
                }
                // The buffer size counts from zero, and crossterm adds one to match unix.
                InputRecord::WindowBufferSizeEvent(size) => self.vt.push(Event::Resize(
                    (i32::from(size.size.x) + 1) as u16,
                    (i32::from(size.size.y) + 1) as u16,
                )),
                // Mouse input arrives as SGR bytes in VT mode. reviewr never asks for focus.
                _ => {}
            }
        }
        self.vt.settle(Instant::now());
        Ok(())
    }
}

/// Wait until the console has input or `timeout` passes. `None` waits indefinitely.
///
/// The one call no safe wrapper offers: crossterm keeps its own wait private, and a reader
/// thread blocked in `ReadConsoleInputW` would steal the keys of an editor given the pane.
#[allow(unsafe_code)]
fn wait_for_input(handle: &Handle, timeout: Option<Duration>) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    // Rounded up, so a wait just short of its deadline sleeps instead of spinning.
    let millis = timeout.map_or(INFINITE, |timeout| {
        u32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(INFINITE - 1)
    });
    // SAFETY: the handle is the open console input handle `Handle` owns and keeps open for this
    // call, and the call takes no pointers.
    match unsafe { WaitForSingleObject((**handle).cast(), millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn set_vt_input(on: bool) {
    let Ok(handle) = Handle::current_in_handle() else { return };
    let mode = ConsoleMode::from(handle);
    if let Ok(before) = mode.mode() {
        let after = if on {
            before | ENABLE_VIRTUAL_TERMINAL_INPUT
        } else {
            before & !ENABLE_VIRTUAL_TERMINAL_INPUT
        };
        let set = mode.set_mode(after);
        crate::logln!("console input mode {before:#x} -> {after:#x} {set:?}");
    }
}

/// Write escape sequences crossterm would route to the console API on Windows.
fn write_out(sequence: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
}
