//! The console's VT input stream, parsed into the events crossterm's unix reader produces.
//!
//! In VT input mode the Windows console hands over the terminal's own byte stream as key
//! records, one UTF-16 unit each: plain text, escape sequences, SGR mouse reports, and a paste
//! between its bracketed-paste markers. `terminput` parses those bytes with crossterm's unix
//! parser, and its crossterm adapter maps the result onto crossterm's `Event`, so the event loop
//! sees a Windows key, click, or paste exactly as it sees one on unix. This module is the byte
//! driver around that parser, which crossterm keeps private: it joins the UTF-16 units, feeds
//! the bytes one at a time, and decides when an incomplete sequence stops waiting.
//!
//! Pure, so the tests run on every OS. Only `windows.rs` calls it in a build.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::Event;

/// How long an open paste may go without a byte before it is taken as complete.
///
/// herdr writes a paste as one write, end marker included, so the bytes arrive back to back.
/// The bound is only for a paste whose end marker never comes: without it every later key
/// would join the paste and input would hang. Half a second is far above any gap inside one
/// write and still short enough to read as a stall.
pub(super) const PASTE_IDLE: Duration = Duration::from_millis(500);

const PASTE_START: &[u8] = b"\x1b[200~";

/// The parser state between console reads.
#[derive(Debug, Default)]
pub(super) struct VtInput {
    /// The bytes of a sequence that has started but not finished.
    pending: Vec<u8>,
    /// A high surrogate whose low half is still to come.
    surrogate: Option<u16>,
    /// Parsed events, oldest first.
    events: VecDeque<Event>,
    /// When the last batch of input arrived, which an open paste's bound counts from.
    last_input: Option<Instant>,
}

impl VtInput {
    /// Take one UTF-16 unit from a key record.
    pub(super) fn feed(&mut self, unit: u16) {
        let ch = match unit {
            0xD800..=0xDBFF => {
                self.surrogate = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => {
                let Some(high) = self.surrogate.take() else { return };
                char::decode_utf16([high, unit]).next().and_then(Result::ok)
            }
            _ => {
                self.surrogate = None;
                char::from_u32(u32::from(unit))
            }
        };
        let mut utf8 = [0; 4];
        for &byte in ch.unwrap_or(char::REPLACEMENT_CHARACTER).encode_utf8(&mut utf8).as_bytes() {
            self.pending.push(byte);
            self.parse(true);
        }
    }

    /// Mark the end of one console read: nothing more is queued behind it.
    ///
    /// A lone ESC waits while more input is queued, since it may open a sequence. With nothing
    /// queued it is the Esc key, the same call crossterm's unix reader makes.
    pub(super) fn settle(&mut self, now: Instant) {
        self.parse(false);
        self.last_input = Some(now);
    }

    /// When an open paste counts as complete, if one is open.
    pub(super) fn paste_deadline(&self) -> Option<Instant> {
        let since = self.last_input?;
        self.pending.starts_with(PASTE_START).then(|| since + PASTE_IDLE)
    }

    /// Close an open paste that has passed its deadline, as a paste of what arrived.
    ///
    /// Never as keys: a lost end marker must not turn pasted text into commands.
    pub(super) fn expire(&mut self, now: Instant) {
        if self.paste_deadline().is_some_and(|deadline| now >= deadline) {
            let text = String::from_utf8_lossy(&self.pending[PASTE_START.len()..]).into_owned();
            self.pending.clear();
            self.events.push_back(Event::Paste(text));
        }
    }

    pub(super) fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    pub(super) fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Queue an event that did not come through the byte stream: a resize.
    pub(super) fn push(&mut self, event: Event) {
        self.events.push_back(event);
    }

    /// Try the pending bytes as one event, the step crossterm's unix reader runs per byte.
    fn parse(&mut self, more: bool) {
        // `parse_from` takes no "more is queued" hint and reads a lone ESC as the Esc key, so the
        // hint is applied here.
        if more && self.pending == b"\x1b" {
            return;
        }
        match terminput::Event::parse_from(&self.pending) {
            Ok(Some(event)) => {
                // Only what crossterm has no type for fails to map, such as an unknown mouse
                // button, and crossterm's own parser drops those too.
                if let Ok(event) = terminput_crossterm::to_crossterm(event) {
                    self.events.push_back(event);
                }
                self.pending.clear();
            }
            Ok(None) => {}
            Err(_) => self.pending.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    /// Feed each batch as one console read and collect every event, in order.
    fn events(vt: &mut VtInput, batches: &[&str], now: Instant) -> Vec<Event> {
        for batch in batches {
            for unit in batch.encode_utf16() {
                vt.feed(unit);
            }
            vt.settle(now);
        }
        std::iter::from_fn(|| vt.pop()).collect()
    }

    fn parse(batches: &[&str]) -> Vec<Event> {
        events(&mut VtInput::default(), batches, Instant::now())
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    #[test]
    fn keys_map_to_the_events_crossterm_reads_on_unix() {
        // The bytes herdr's ConPTY delivered for each key in the Windows VM, legacy encoding
        // first, then with the kitty protocol's disambiguate flag pushed.
        let rows: &[(&str, Event)] = &[
            ("a", key(KeyCode::Char('a'), NONE)),
            ("A", key(KeyCode::Char('A'), SHIFT)),
            ("é", key(KeyCode::Char('é'), NONE)),
            ("日", key(KeyCode::Char('日'), NONE)),
            ("🙂", key(KeyCode::Char('🙂'), NONE)),
            ("\r", key(KeyCode::Enter, NONE)),
            ("\n", key(KeyCode::Char('j'), CONTROL)),
            ("\x7f", key(KeyCode::Backspace, NONE)),
            ("\x1b", key(KeyCode::Esc, NONE)),
            ("\x1b\r", key(KeyCode::Enter, ALT)),
            ("\x1bx", key(KeyCode::Char('x'), ALT)),
            ("\x1b[A", key(KeyCode::Up, NONE)),
            ("\x1b[D", key(KeyCode::Left, NONE)),
            ("\x1b[1;5D", key(KeyCode::Left, CONTROL)),
            ("\x1b[Z", key(KeyCode::BackTab, SHIFT)),
            ("\x1b[13;2u", key(KeyCode::Enter, SHIFT)),
            ("\x1b[13;3u", key(KeyCode::Enter, ALT)),
            ("\x1b[106;5u", key(KeyCode::Char('j'), CONTROL)),
            ("\x1b[27u", key(KeyCode::Esc, NONE)),
            ("\x1b[120;3u", key(KeyCode::Char('x'), ALT)),
            ("\x1b[9;2u", key(KeyCode::BackTab, SHIFT)),
        ];
        for (bytes, expected) in rows {
            assert_eq!(parse(&[bytes]), vec![expected.clone()], "{bytes:?}");
        }
    }

    #[test]
    fn sgr_mouse_reports_map_to_zero_based_mouse_events() {
        let rows: &[(&str, Event)] = &[
            ("\x1b[<0;10;5M", mouse(MouseEventKind::Down(MouseButton::Left), 9, 4)),
            ("\x1b[<0;10;5m", mouse(MouseEventKind::Up(MouseButton::Left), 9, 4)),
            ("\x1b[<32;11;5M", mouse(MouseEventKind::Drag(MouseButton::Left), 10, 4)),
            ("\x1b[<64;10;5M", mouse(MouseEventKind::ScrollUp, 9, 4)),
            ("\x1b[<65;10;5M", mouse(MouseEventKind::ScrollDown, 9, 4)),
        ];
        for (bytes, expected) in rows {
            assert_eq!(parse(&[bytes]), vec![expected.clone()], "{bytes:?}");
        }
    }

    #[test]
    fn a_bracketed_paste_is_one_event_with_its_text_verbatim() {
        // Exactly what herdr's paste handler writes on Windows: markers, CRLF line breaks.
        let pasted = "\x1b[200~line one\r\nline two é 日本 🙂\x1b[201~";
        assert_eq!(parse(&[pasted]), vec![Event::Paste("line one\r\nline two é 日本 🙂".into())]);
        // Split across console reads, it is still one paste.
        assert_eq!(
            parse(&["\x1b[200~line one\r", "\nline two\x1b[2", "01~"]),
            vec![Event::Paste("line one\r\nline two".into())]
        );
    }

    #[test]
    fn a_sequence_split_across_reads_waits_for_its_end() {
        assert_eq!(parse(&["\x1b[", "A"]), vec![key(KeyCode::Up, NONE)]);
        // A surrogate pair split across reads is one character.
        let smile: Vec<u16> = "🙂".encode_utf16().collect();
        let mut vt = VtInput::default();
        vt.feed(smile[0]);
        vt.settle(Instant::now());
        vt.feed(smile[1]);
        vt.settle(Instant::now());
        assert_eq!(vt.pop(), Some(key(KeyCode::Char('🙂'), NONE)));
    }

    #[test]
    fn a_lone_esc_at_the_end_of_a_read_is_the_esc_key() {
        assert_eq!(
            parse(&["\x1b", "j"]),
            vec![key(KeyCode::Esc, NONE), key(KeyCode::Char('j'), NONE)]
        );
    }

    #[test]
    fn an_unterminated_paste_closes_as_a_paste_after_its_bound() {
        let mut vt = VtInput::default();
        let start = Instant::now();
        assert_eq!(events(&mut vt, &["\x1b[200~abc\rx"], start), vec![]);
        vt.expire(start + PASTE_IDLE.saturating_sub(Duration::from_millis(1)));
        assert_eq!(vt.pop(), None, "still inside the bound");
        vt.expire(start + PASTE_IDLE);
        assert_eq!(vt.pop(), Some(Event::Paste("abc\rx".into())));
        // Input reads as keys again.
        assert_eq!(events(&mut vt, &["x"], start), vec![key(KeyCode::Char('x'), NONE)]);
    }
}
