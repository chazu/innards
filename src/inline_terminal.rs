use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{Context, Result, anyhow};
use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use signal_hook::{consts::TERM_SIGNALS, flag};

const MIN_HEIGHT: u16 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeStep {
    Grow,
    Shrink,
}

impl ResizeStep {
    /// C-x ^ / C-x - share the existing Alt-Down / Alt-Up resize behavior.
    /// Bare punctuation remains available to editors and filter inputs.
    pub fn from_key(key: KeyEvent, after_ctrl_x: bool) -> Option<Self> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        match key.code {
            KeyCode::Char('^')
                if after_ctrl_x
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
            {
                Some(Self::Grow)
            }
            KeyCode::Char('-') if after_ctrl_x && key.modifiers.is_empty() => Some(Self::Shrink),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => Some(Self::Shrink),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => Some(Self::Grow),
            _ => None,
        }
    }

    fn height(self, current: u16, minimum: u16, rows: u16) -> u16 {
        let requested = match self {
            Self::Grow => current.saturating_add(1),
            Self::Shrink => current.saturating_sub(1),
        };
        requested.max(minimum).min(rows.max(1))
    }
}

/// Inline terminal surface backed by the controlling terminal, never stdout.
///
/// Stdin and stdout therefore remain available to a caller as data and result
/// channels while the surface owns `/dev/tty` for its lifetime.
pub struct InlineTerminal {
    terminal: Terminal<TtyBackend>,
    tty: File,
    mouse_capture: bool,
    bracketed_paste: bool,
    area: Rect,
    resize_prefix_pending: bool,
}

impl InlineTerminal {
    pub fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let tty = match open_terminal_device() {
            Ok(tty) => tty,
            Err(err) => {
                let _ = disable_raw_mode();
                return Err(err);
            }
        };
        let terminal = match Self::new_terminal(&tty, height, None) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                return Err(err);
            }
        };
        Ok(Self {
            terminal,
            tty,
            mouse_capture: false,
            bracketed_paste: false,
            area: Rect::new(0, 0, 0, height.max(MIN_HEIGHT)),
            resize_prefix_pending: false,
        })
    }

    pub fn enable_mouse_capture(&mut self) -> Result<()> {
        // Set this first so Drop also cleans up a partially written command.
        self.mouse_capture = true;
        self.tty.execute(EnableMouseCapture)?;
        Ok(())
    }

    pub fn enable_bracketed_paste(&mut self) -> Result<()> {
        self.bracketed_paste = true;
        self.tty.execute(EnableBracketedPaste)?;
        Ok(())
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        let area = &mut self.area;
        self.terminal.draw(|frame| {
            *area = frame.area();
            render(frame);
        })?;
        Ok(())
    }

    pub fn height(&self) -> u16 {
        self.area.height
    }

    /// Consume only resize chords and their prefix. Other keys retain the
    /// surface's normal behavior, including cancellation after a prefix.
    pub fn handle_resize_key(&mut self, key: KeyEvent, minimum: u16) -> Result<bool> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }
        let step = ResizeStep::from_key(key, self.resize_prefix_pending);
        self.resize_prefix_pending =
            key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(step) = step {
            self.resize_by(step, minimum)?;
            return Ok(true);
        }
        Ok(self.resize_prefix_pending)
    }

    pub fn resize_by(&mut self, step: ResizeStep, minimum: u16) -> Result<()> {
        let rows = terminal_rows(self.area.height);
        let height = step.height(self.area.height, minimum, rows);
        if height != self.area.height {
            let anchor_y = resize_anchor_row(self.area.y, self.area.height, height, rows);
            self.resize(height, anchor_y)?;
        }
        Ok(())
    }

    pub fn resize(&mut self, height: u16, anchor_y: u16) -> Result<()> {
        self.terminal.clear()?;
        self.tty.execute(MoveTo(0, anchor_y))?;
        // MoveTo gives us the exact cursor position. Querying it again would
        // consume queued input while waiting for a terminal reply during resize.
        self.terminal = Self::new_terminal(&self.tty, height, Some(Position::new(0, anchor_y)))?;
        self.area.y = anchor_y;
        self.area.height = height;
        Ok(())
    }

    fn new_terminal(
        tty: &File,
        height: u16,
        cursor_position: Option<Position>,
    ) -> Result<Terminal<TtyBackend>> {
        let mut terminal_io = tty.try_clone()?;
        let cursor_position = match cursor_position {
            Some(position) => position,
            None => query_cursor_position(&mut terminal_io)?,
        };
        let backend = TtyBackend {
            inner: CrosstermBackend::new(terminal_io),
            cursor_position,
        };
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(MIN_HEIGHT).min(terminal_rows(height))),
            },
        )?;
        Ok(terminal)
    }
}

fn terminal_rows(fallback: u16) -> u16 {
    size()
        .ok()
        .map(|(_, rows)| rows)
        .filter(|rows| *rows > 0)
        .unwrap_or(fallback.max(MIN_HEIGHT))
}

fn resize_anchor_row(
    previous_top: u16,
    previous_height: u16,
    new_height: u16,
    terminal_rows: u16,
) -> u16 {
    let anchor = if new_height < previous_height {
        previous_top.saturating_add(previous_height - new_height)
    } else {
        previous_top
    };
    anchor.min(terminal_rows.saturating_sub(1))
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
        if self.bracketed_paste {
            let _ = self.tty.execute(DisableBracketedPaste);
        }
        if self.mouse_capture {
            let _ = self.tty.execute(DisableMouseCapture);
        }
        let _ = self.terminal.clear();
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        let _ = self.tty.flush();
    }
}

pub fn termination_flag() -> Result<Arc<AtomicBool>> {
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in TERM_SIGNALS {
        flag::register(*signal, Arc::clone(&interrupted))?;
    }
    Ok(interrupted)
}

fn open_terminal_device() -> Result<File> {
    #[cfg(unix)]
    let path = "/dev/tty";
    #[cfg(windows)]
    let path = "CONOUT$";

    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open terminal device {path}"))
}

struct TtyBackend {
    inner: CrosstermBackend<File>,
    cursor_position: Position,
}

impl Backend for TtyBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor_position)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.cursor_position = position;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(unix)]
fn query_cursor_position(tty: &mut File) -> Result<Position> {
    tty.write_all(b"\x1b[6n")?;
    tty.flush()?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut response = Vec::with_capacity(16);
    while Instant::now() < deadline && response.len() < 64 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for the duration
        // of the call, and tty remains open throughout the loop.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("failed while waiting for cursor position");
        }
        if ready == 0 {
            break;
        }

        let mut byte = [0_u8; 1];
        tty.read_exact(&mut byte)?;
        response.push(byte[0]);
        if byte[0] == b'R' {
            return parse_cursor_position(&response);
        }
    }

    Err(anyhow!("terminal did not report its cursor position"))
}

#[cfg(unix)]
fn parse_cursor_position(response: &[u8]) -> Result<Position> {
    let response = std::str::from_utf8(response).context("cursor position was not UTF-8")?;
    let start = response
        .rfind("\x1b[")
        .ok_or_else(|| anyhow!("cursor position response had no CSI prefix"))?;
    let coordinates = response[start + 2..]
        .strip_suffix('R')
        .ok_or_else(|| anyhow!("cursor position response had no terminator"))?;
    let (row, column) = coordinates
        .split_once(';')
        .ok_or_else(|| anyhow!("cursor position response had no separator"))?;
    let row = row.parse::<u16>().context("invalid cursor row")?;
    let column = column.parse::<u16>().context("invalid cursor column")?;
    if row == 0 || column == 0 {
        return Err(anyhow!("cursor position must be one-based"));
    }
    Ok(Position::new(column - 1, row - 1))
}

#[cfg(windows)]
fn query_cursor_position(_tty: &mut File) -> Result<Position> {
    crossterm::cursor::position()
        .map(|(x, y)| Position::new(x, y))
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_keys_require_a_prefix_or_alt_arrow() {
        let caret = KeyEvent::new(KeyCode::Char('^'), KeyModifiers::SHIFT);
        let minus = KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE);
        assert_eq!(ResizeStep::from_key(caret, false), None);
        assert_eq!(ResizeStep::from_key(minus, false), None);
        assert_eq!(ResizeStep::from_key(caret, true), Some(ResizeStep::Grow));
        assert_eq!(ResizeStep::from_key(minus, true), Some(ResizeStep::Shrink));
        assert_eq!(
            ResizeStep::from_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), false),
            Some(ResizeStep::Shrink)
        );
        assert_eq!(
            ResizeStep::from_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), false),
            Some(ResizeStep::Grow)
        );
        assert_eq!(
            ResizeStep::from_key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                true
            ),
            None,
            "C-x C-s must remain available to the editor"
        );
    }

    #[test]
    fn resize_accepts_repeats_but_not_release_events() {
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(
            ResizeStep::from_key(
                KeyEvent {
                    kind: KeyEventKind::Repeat,
                    ..key
                },
                false
            ),
            Some(ResizeStep::Grow)
        );
        assert_eq!(
            ResizeStep::from_key(
                KeyEvent {
                    kind: KeyEventKind::Release,
                    ..key
                },
                false
            ),
            None
        );
    }

    #[test]
    fn height_changes_one_row_and_respects_layout_and_terminal_limits() {
        assert_eq!(ResizeStep::Grow.height(12, 5, 24), 13);
        assert_eq!(ResizeStep::Shrink.height(12, 5, 24), 11);
        assert_eq!(ResizeStep::Shrink.height(5, 5, 24), 5);
        assert_eq!(ResizeStep::Shrink.height(10, 10, 24), 10);
        assert_eq!(ResizeStep::Grow.height(24, 5, 24), 24);
        assert_eq!(ResizeStep::Shrink.height(3, 5, 3), 3);
        assert_eq!(ResizeStep::Grow.height(3, 5, 3), 3);
        assert_eq!(ResizeStep::Grow.height(u16::MAX, 5, 24), 24);
    }

    #[test]
    fn resize_anchor_preserves_top_when_growing() {
        assert_eq!(resize_anchor_row(8, 16, 17, 24), 8);
    }

    #[test]
    fn resize_anchor_preserves_bottom_when_shrinking() {
        assert_eq!(resize_anchor_row(8, 16, 12, 24), 12);
    }

    #[cfg(unix)]
    #[test]
    fn cursor_position_response_is_converted_to_zero_based_coordinates() {
        assert_eq!(
            parse_cursor_position(b"\x1b[12;34R").unwrap(),
            Position::new(33, 11)
        );
    }

    #[cfg(unix)]
    #[test]
    fn cursor_position_response_rejects_zero_coordinates() {
        let error = parse_cursor_position(b"\x1b[0;1R").unwrap_err();

        assert!(error.to_string().contains("one-based"));
    }
}
