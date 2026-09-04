use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use serde::{Deserialize, Serialize};

use crate::inline_terminal::{InlineTerminal, termination_flag};

const DEFAULT_HEIGHT: u16 = 20;
const MIN_HEIGHT: u16 = 7;

#[derive(Clone, Debug)]
pub struct Config {
    pub height: u16,
    pub title: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            height: DEFAULT_HEIGHT,
            title: "indiff".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Rejected,
    Cancelled,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Accepted => 0,
            Self::Rejected => 3,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewResult {
    pub schema_version: u8,
    pub outcome: Outcome,
    pub accepted_hunks: Vec<usize>,
    pub rejected_hunks: Vec<usize>,
}

pub fn write_result_json(result: &ReviewResult) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, result)?;
    writeln!(stdout)?;
    Ok(())
}

pub fn run_with(diff: String, config: Config) -> Result<ReviewResult> {
    let interrupted = termination_flag()?;
    let mut app = App::new(diff, config)?;
    let mut terminal = InlineTerminal::enter(app.config.height.max(MIN_HEIGHT))?;

    loop {
        if interrupted.load(Ordering::Relaxed) {
            drop(terminal);
            return Ok(app.result(Outcome::Cancelled));
        }
        terminal.draw(|frame| draw(frame, &app))?;

        let ready = match event::poll(Duration::from_millis(80)) {
            Ok(ready) => ready,
            Err(_) if interrupted.load(Ordering::Relaxed) => {
                drop(terminal);
                return Ok(app.result(Outcome::Cancelled));
            }
            Err(error) => return Err(error.into()),
        };
        if !ready {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if let Some(outcome) = handle_key(&mut app, key) {
            drop(terminal);
            return Ok(app.result(outcome));
        }
    }
}

struct App {
    config: Config,
    headers: Vec<String>,
    hunks: Vec<Vec<String>>,
    current_hunk: usize,
    accepted_hunks: Vec<usize>,
    rejected_hunks: Vec<usize>,
    scroll: usize,
}

impl App {
    fn new(diff: String, config: Config) -> Result<Self> {
        let (headers, hunks) = split_diff(&diff)?;
        Ok(Self {
            config,
            headers,
            hunks,
            current_hunk: 0,
            accepted_hunks: Vec::new(),
            rejected_hunks: Vec::new(),
            scroll: 0,
        })
    }

    fn result(&self, outcome: Outcome) -> ReviewResult {
        ReviewResult {
            schema_version: 1,
            outcome,
            accepted_hunks: self.accepted_hunks.clone(),
            rejected_hunks: self.rejected_hunks.clone(),
        }
    }

    fn visible_lines(&self) -> Vec<&str> {
        self.headers
            .iter()
            .chain(self.hunks[self.current_hunk].iter())
            .map(String::as_str)
            .collect()
    }

    fn move_down(&mut self, amount: usize) {
        self.scroll = (self.scroll + amount).min(self.visible_lines().len().saturating_sub(1));
    }

    fn decide_current(&mut self, accepted: bool) -> Option<Outcome> {
        if accepted {
            self.accepted_hunks.push(self.current_hunk);
        } else {
            self.rejected_hunks.push(self.current_hunk);
        }
        if self.current_hunk + 1 == self.hunks.len() {
            return Some(if self.accepted_hunks.is_empty() {
                Outcome::Rejected
            } else {
                Outcome::Accepted
            });
        }
        self.current_hunk += 1;
        self.scroll = 0;
        None
    }

    fn decide_remaining(&mut self, accepted: bool) -> Outcome {
        for index in self.current_hunk..self.hunks.len() {
            if accepted {
                self.accepted_hunks.push(index);
            } else {
                self.rejected_hunks.push(index);
            }
        }
        if self.accepted_hunks.is_empty() {
            Outcome::Rejected
        } else {
            Outcome::Accepted
        }
    }
}

fn split_diff(diff: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let mut headers = Vec::new();
    let mut hunks: Vec<Vec<String>> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@ ") {
            hunks.push(vec![line.to_string()]);
        } else if let Some(hunk) = hunks.last_mut() {
            hunk.push(line.to_string());
        } else {
            headers.push(line.to_string());
        }
    }
    if hunks.is_empty() {
        return Err(anyhow::anyhow!(
            "indiff requires at least one unified-diff hunk"
        ));
    }
    Ok((headers, hunks))
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<Outcome> {
    match key.code {
        KeyCode::Enter | KeyCode::Char('a' | 'y') => app.decide_current(true),
        KeyCode::Char('A') => Some(app.decide_remaining(true)),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Outcome::Cancelled)
        }
        KeyCode::Up => {
            app.scroll = app.scroll.saturating_sub(1);
            None
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll = app.scroll.saturating_sub(1);
            None
        }
        KeyCode::Down => {
            app.move_down(1);
            None
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.move_down(1);
            None
        }
        KeyCode::Esc | KeyCode::Char('q') => Some(app.decide_remaining(false)),
        KeyCode::Char('r' | 'n') => app.decide_current(false),
        KeyCode::Char('R') => Some(app.decide_remaining(false)),
        KeyCode::PageUp => {
            app.scroll = app.scroll.saturating_sub(10);
            None
        }
        KeyCode::PageDown => {
            app.move_down(10);
            None
        }
        _ => None,
    }
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    frame.render_widget(Clear, frame.area());
    let all_lines = app.visible_lines();
    let visible = usize::from(frame.area().height.saturating_sub(3)).max(1);
    let end = (app.scroll + visible).min(all_lines.len());
    let lines: Vec<Line<'_>> = all_lines[app.scroll..end]
        .iter()
        .map(|line| styled_diff_line(line))
        .collect();
    let title = format!(
        " {} (hunk {}/{}) ",
        app.config.title,
        app.current_hunk + 1,
        app.hunks.len()
    );
    let review = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(review, frame.area());

    let status = "a/y accept hunk | r/n reject hunk | A/R all | q/esc reject all | ctrl-c cancel";
    let status_area = ratatui::layout::Rect {
        x: frame.area().x.saturating_add(1),
        y: frame.area().bottom().saturating_sub(2),
        width: frame.area().width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        status_area,
    );
}

fn styled_diff_line(line: &str) -> Line<'_> {
    let color = if line.starts_with("+++") || line.starts_with("---") {
        Color::Yellow
    } else if line.starts_with('+') {
        Color::Green
    } else if line.starts_with('-') {
        Color::Red
    } else if line.starts_with("@@") {
        Color::Cyan
    } else {
        Color::White
    };
    Line::from(Span::styled(line, Style::default().fg(color)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_have_stable_exit_codes() {
        assert_eq!(Outcome::Accepted.exit_code(), 0);
        assert_eq!(Outcome::Rejected.exit_code(), 3);
        assert_eq!(Outcome::Cancelled.exit_code(), 130);
    }

    #[test]
    fn navigation_does_not_decide() {
        let mut app = App::new(
            "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-one\n+two".to_string(),
            Config::default(),
        )
        .unwrap();
        assert_eq!(
            handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            None
        );
        assert_eq!(app.scroll, 1);
    }

    #[test]
    fn accept_and_reject_are_explicit() {
        let diff = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-one\n+two";
        let mut app = App::new(diff.to_string(), Config::default()).unwrap();
        assert_eq!(
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
            ),
            Some(Outcome::Accepted)
        );
        assert_eq!(app.accepted_hunks, vec![0]);
        let mut app = App::new(diff.to_string(), Config::default()).unwrap();
        assert_eq!(
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)
            ),
            Some(Outcome::Rejected)
        );
        assert_eq!(app.rejected_hunks, vec![0]);
    }

    #[test]
    fn decisions_advance_hunk_by_hunk() {
        let diff = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-one\n+two\n@@ -4 +4 @@\n-three\n+four";
        let mut app = App::new(diff.to_string(), Config::default()).unwrap();
        assert_eq!(app.decide_current(false), None);
        assert_eq!(app.current_hunk, 1);
        assert_eq!(app.decide_current(true), Some(Outcome::Accepted));
        assert_eq!(app.accepted_hunks, vec![1]);
        assert_eq!(app.rejected_hunks, vec![0]);
    }
}
