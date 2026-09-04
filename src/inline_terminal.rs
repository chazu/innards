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
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use signal_hook::{consts::TERM_SIGNALS, flag};

const MIN_HEIGHT: u16 = 5;

/// Inline terminal surface backed by the controlling terminal, never stdout.
///
/// Stdin and stdout therefore remain available to a caller as data and result
/// channels while the surface owns `/dev/tty` for its lifetime.
pub struct InlineTerminal {
    terminal: Terminal<TtyBackend>,
    tty: File,
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
        let terminal = match Self::new_terminal(&tty, height) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                return Err(err);
            }
        };
        Ok(Self { terminal, tty })
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }

    pub fn resize(&mut self, height: u16, anchor_y: u16) -> Result<()> {
        self.terminal.clear()?;
        self.tty.execute(MoveTo(0, anchor_y))?;
        self.terminal = Self::new_terminal(&self.tty, height)?;
        Ok(())
    }

    fn new_terminal(tty: &File, height: u16) -> Result<Terminal<TtyBackend>> {
        let mut terminal_io = tty.try_clone()?;
        let cursor_position = query_cursor_position(&mut terminal_io)?;
        let backend = TtyBackend {
            inner: CrosstermBackend::new(terminal_io),
            cursor_position,
        };
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(MIN_HEIGHT)),
            },
        )?;
        Ok(terminal)
    }
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
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
