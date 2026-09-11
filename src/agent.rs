//! Presentation only: snapshots in, explicit intents out, terminal on /dev/tty.
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::{atomic::Ordering, mpsc};
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::inline_terminal::{InlineTerminal, ResizeStep, termination_flag};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub profile: String,
    pub lifecycle: String,
    pub activity: String,
    pub run_id: String,
    pub pending: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Snapshot {
        schema_version: u8,
        session: Session,
        entries: Vec<Entry>,
        has_earlier: bool,
        window: usize,
    },
    Ack {
        schema_version: u8,
        request_id: u64,
        ok: bool,
        message: String,
    },
}

pub fn parse_input(line: &str) -> Result<Input> {
    let input: Input = serde_json::from_str(line)?;
    let version = match &input {
        Input::Snapshot { schema_version, .. } | Input::Ack { schema_version, .. } => {
            *schema_version
        }
    };
    if version != 1 {
        bail!("Unsupported agent view schema {version}");
    }
    if let Input::Snapshot {
        session, entries, ..
    } = &input
    {
        if session.id.is_empty() || entries.iter().any(|e| e.id.is_empty()) {
            bail!("Snapshot requires stable session and entry IDs");
        }
        let mut ids = std::collections::HashSet::new();
        if entries.iter().any(|e| !ids.insert(&e.id)) {
            bail!("Duplicate transcript entry ID");
        }
    }
    Ok(input)
}

#[derive(Default, Clone)]
struct Draft {
    chars: Vec<char>,
    cursor: usize,
    killed: Vec<char>,
}
struct DraftLayout {
    rows: Vec<String>,
    cursor: (usize, usize),
}
impl Draft {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    // Soft wrapping is presentation only. Use the same cell layout for visible
    // rows and the cursor, keeping graphemes intact and hard newlines unchanged.
    fn layout(&self, width: usize) -> DraftLayout {
        let width = width.max(1);
        let text = display_text(&self.text());
        let mut rows = vec![String::new()];
        let mut cursor = (0, 0);
        let mut offset = 0;
        let mut col = 0;
        for (line_index, line) in text.split('\n').enumerate() {
            if line_index > 0 {
                rows.push(String::new());
                col = 0;
                offset += 1;
            }
            let span = Span::raw(line);
            for grapheme in span.styled_graphemes(Style::default()) {
                let chars = grapheme.symbol.chars().count();
                let cells = Span::raw(grapheme.symbol).width();
                let symbol = if cells > width {
                    "�"
                } else {
                    grapheme.symbol
                };
                let cells = cells.min(width);
                if col > 0 && col + cells > width {
                    rows.push(String::new());
                    col = 0;
                }
                if (offset..offset + chars).contains(&self.cursor) {
                    cursor = (col, rows.len() - 1);
                }
                rows.last_mut().unwrap().push_str(symbol);
                col += cells;
                offset += chars;
            }
            if self.cursor == offset {
                cursor = if col == width {
                    (0, rows.len())
                } else {
                    (col, rows.len() - 1)
                };
            }
        }
        if cursor.1 == rows.len() {
            rows.push(String::new());
        }
        DraftLayout { rows, cursor }
    }
    fn insert(&mut self, text: &str) {
        let chars: Vec<_> = text.chars().collect();
        let n = chars.len();
        self.chars.splice(self.cursor..self.cursor, chars);
        self.cursor += n;
    }
    fn start(&self) -> usize {
        self.chars[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |p| p + 1)
    }
    fn end(&self) -> usize {
        self.chars[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.chars.len(), |p| self.cursor + p)
    }
    fn vertical(&mut self, up: bool) {
        let start = self.start();
        let col = self.cursor - start;
        if up && start > 0 {
            let end = start - 1;
            let previous = self.chars[..end]
                .iter()
                .rposition(|c| *c == '\n')
                .map_or(0, |p| p + 1);
            self.cursor = (previous + col).min(end);
        }
        if !up && self.end() < self.chars.len() {
            self.cursor = self.end() + 1;
            self.cursor = (self.cursor + col).min(self.end());
        }
    }
    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = self.start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.end(),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Char('p') if ctrl => self.vertical(true),
            KeyCode::Char('n') if ctrl => self.vertical(false),
            KeyCode::Char('k') if ctrl => {
                let mut end = self.end();
                if end == self.cursor && end < self.chars.len() {
                    end += 1;
                }
                self.killed = self.chars.drain(self.cursor..end).collect();
            }
            KeyCode::Char('y') if ctrl => self.insert(&self.killed.iter().collect::<String>()),
            KeyCode::Char('d') if ctrl => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(&c.to_string())
            }
            KeyCode::Enter => self.insert("\n"),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Up => self.vertical(true),
            KeyCode::Down => self.vertical(false),
            KeyCode::Home => self.cursor = self.start(),
            KeyCode::End => self.cursor = self.end(),
            _ => {}
        }
    }
}

#[derive(Clone)]
struct Row {
    id: String,
    text: String,
    color: Color,
}
enum Overlay {
    Commands(usize),
    ConfirmStop(String),
    DiscardDraft,
    Search(Draft),
}
const COMMANDS: &[&str] = &[
    "Follow latest output",
    "Load earlier history",
    "Pause new work",
    "Resume queued work",
    "Stop displayed run",
    "Detach",
];

pub struct App {
    session: Session,
    entries: Vec<Entry>,
    rows: Vec<Row>,
    draft: Draft,
    scroll: usize,
    follow: bool,
    composing: bool,
    earlier: bool,
    window: usize,
    connected: bool,
    status: String,
    overlay: Option<Overlay>,
    ctrl_x: bool,
    ctrl_c: bool,
    next_request: u64,
    pending: Option<(u64, String)>,
    width: usize,
    page: usize,
    viewed: HashSet<String>,
    read_requests: HashSet<u64>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            session: Session::default(),
            entries: vec![],
            rows: vec![],
            draft: Draft::default(),
            scroll: 0,
            follow: true,
            composing: true,
            earlier: false,
            window: 0,
            connected: true,
            status: "Waiting for session…".into(),
            overlay: None,
            ctrl_x: false,
            ctrl_c: false,
            next_request: 0,
            pending: None,
            width: 80,
            page: 12,
            viewed: HashSet::new(),
            read_requests: HashSet::new(),
        }
    }
}
impl App {
    pub fn apply(&mut self, input: Input) -> Result<()> {
        match input {
            Input::Snapshot {
                session,
                entries,
                has_earlier,
                window,
                ..
            } => {
                if !self.session.id.is_empty() && self.session.id != session.id {
                    bail!("Bridge attempted to change the attached session");
                }
                self.session = session;
                self.entries = entries;
                self.earlier = has_earlier;
                self.window = window;
                self.rebuild(self.width);
                if self.status == "Waiting for session…" {
                    self.status = "Attached · messages use the agent inbox".into();
                }
            }
            Input::Ack {
                request_id,
                ok,
                message,
                ..
            } => {
                if self.read_requests.remove(&request_id) && ok {
                    return Ok(());
                }
                if let Some((id, body)) = &self.pending {
                    if *id == request_id {
                        if ok && self.draft.text() == *body {
                            self.draft = Draft::default();
                        }
                        self.pending = None;
                    }
                }
                self.status = if ok {
                    message
                } else {
                    format!("Not applied: {message}")
                };
            }
        }
        Ok(())
    }
    fn rebuild(&mut self, width: usize) {
        let anchor = self.rows.get(self.scroll).map(|r| r.id.clone());
        self.width = width.max(1);
        self.rows.clear();
        for entry in &self.entries {
            let color = match entry.kind.as_str() {
                "message" | "question" => Color::Cyan,
                "error" => Color::Red,
                "tool" | "tool_input_delta" => Color::Yellow,
                "reasoning" | "reasoning_delta" | "status" => Color::DarkGray,
                _ => Color::White,
            };
            let text = format!("{}\n{}\n", entry.title, entry.text);
            for (i, line) in wrap(&text, self.width).into_iter().enumerate() {
                self.rows.push(Row {
                    id: format!("{}/{i}", entry.id),
                    text: line,
                    color,
                });
            }
        }
        if self.follow {
            self.scroll = self.bottom();
        } else if let Some(anchor) = anchor {
            if let Some(index) = self.rows.iter().position(|r| r.id == anchor) {
                self.scroll = index;
            }
        }
        self.scroll = self.scroll.min(self.bottom());
    }
    fn bottom(&self) -> usize {
        self.rows.len().saturating_sub(self.page)
    }

    fn viewed_intent(&mut self) -> Option<Value> {
        if !self.connected {
            return None;
        }
        let visible: HashSet<_> = self
            .rows
            .iter()
            .skip(self.scroll)
            .take(self.page)
            .filter_map(|row| row.id.rsplit_once('/').map(|(id, _)| id.to_owned()))
            .collect();
        let ids: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                matches!(entry.kind.as_str(), "message" | "question")
                    && visible.contains(&entry.id)
                    && !self.viewed.contains(&entry.id)
            })
            .take(200)
            .map(|entry| entry.id.clone())
            .collect();
        if ids.is_empty() {
            return None;
        }
        self.viewed.extend(ids.clone());
        let intent = self.request("mark_viewed", json!({"message_ids":ids}));
        self.read_requests.insert(self.next_request);
        Some(intent)
    }
    fn request(&mut self, intent: &str, fields: Value) -> Value {
        self.next_request += 1;
        let mut value = json!({"schema_version":1,"request_id":self.next_request,"intent":intent});
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        value
    }
    fn detach(&mut self) -> Option<Value> {
        if self.draft.chars.is_empty() {
            Some(self.request("dismiss", json!({})))
        } else {
            self.overlay = Some(Overlay::DiscardDraft);
            None
        }
    }
    fn older(&mut self) -> Option<Value> {
        self.follow = false;
        if self.earlier {
            Some(self.request("load_older", json!({})))
        } else {
            self.scroll = 0;
            None
        }
    }
    fn search(&mut self, query: &str, reverse: bool) {
        if query.is_empty() {
            return;
        }
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let index = if reverse {
                (self.scroll + n - step % n) % n
            } else {
                (self.scroll + step) % n
            };
            if self.rows[index]
                .text
                .to_lowercase()
                .contains(&query.to_lowercase())
            {
                self.scroll = index.min(self.bottom());
                self.follow = false;
                return;
            }
        }
        self.status = format!("No match for {query}");
    }
    pub fn key(&mut self, key: KeyEvent) -> Option<Value> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let previous_x = std::mem::take(&mut self.ctrl_x);
        let previous_c = std::mem::take(&mut self.ctrl_c);
        if previous_x && ctrl && key.code == KeyCode::Char('c') {
            return self.detach();
        }
        if ctrl && key.code == KeyCode::Char('x') {
            self.ctrl_x = true;
            return None;
        }
        if let Some(overlay) = self.overlay.take() {
            match overlay {
                Overlay::DiscardDraft => {
                    if key.code == KeyCode::Char('y') {
                        return Some(self.request("dismiss", json!({})));
                    }
                    if key.code != KeyCode::Char('n')
                        && key.code != KeyCode::Esc
                        && !(ctrl && key.code == KeyCode::Char('g'))
                    {
                        self.overlay = Some(Overlay::DiscardDraft);
                    }
                }
                Overlay::ConfirmStop(run) => {
                    if key.code == KeyCode::Char('y') {
                        return Some(self.request("interrupt_run", json!({"run_id":run})));
                    }
                    if key.code != KeyCode::Char('n')
                        && key.code != KeyCode::Esc
                        && !(ctrl && key.code == KeyCode::Char('g'))
                    {
                        self.overlay = Some(Overlay::ConfirmStop(run));
                    }
                }
                Overlay::Commands(mut index) => {
                    if key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n')) {
                        index = (index + 1) % COMMANDS.len();
                    }
                    if key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p')) {
                        index = (index + COMMANDS.len() - 1) % COMMANDS.len();
                    }
                    if key.code == KeyCode::Enter {
                        match index {
                            0 => {
                                self.follow = true;
                                self.scroll = self.bottom();
                            }
                            1 => return self.older(),
                            2 => return Some(self.request("pause_session", json!({}))),
                            3 => return Some(self.request("resume_session", json!({}))),
                            4 => {
                                if self.session.run_id.is_empty() {
                                    self.status = "No active run".into();
                                } else {
                                    self.overlay =
                                        Some(Overlay::ConfirmStop(self.session.run_id.clone()));
                                }
                            }
                            _ => return self.detach(),
                        }
                    } else if key.code != KeyCode::Esc && !(ctrl && key.code == KeyCode::Char('g'))
                    {
                        self.overlay = Some(Overlay::Commands(index));
                    }
                }
                Overlay::Search(mut query) => {
                    if key.code == KeyCode::Esc
                        || key.code == KeyCode::Enter
                        || (ctrl && key.code == KeyCode::Char('g'))
                    {
                        return None;
                    }
                    if ctrl && (key.code == KeyCode::Char('s') || key.code == KeyCode::Char('r')) {
                        self.search(&query.text(), key.code == KeyCode::Char('r'));
                    } else {
                        query.key(key);
                        self.search(&query.text(), false);
                    }
                    self.overlay = Some(Overlay::Search(query));
                }
            }
            return None;
        }
        if alt && key.code == KeyCode::Char('x') {
            self.overlay = Some(Overlay::Commands(0));
            return None;
        }
        if ctrl && (key.code == KeyCode::Char('s') || key.code == KeyCode::Char('r')) {
            self.overlay = Some(Overlay::Search(Draft::default()));
            self.composing = false;
            return None;
        }
        if key.code == KeyCode::Tab {
            self.composing = !self.composing;
            return None;
        }
        if alt && key.code == KeyCode::Char('>') {
            self.follow = true;
            self.scroll = self.bottom();
            return None;
        }
        if alt && key.code == KeyCode::Char('<') {
            self.scroll = 0;
            return self.older();
        }
        if key.code == KeyCode::PageUp || (alt && key.code == KeyCode::Char('v')) {
            self.follow = false;
            if self.scroll == 0 {
                return self.older();
            }
            self.scroll = self.scroll.saturating_sub(self.page);
            return None;
        }
        if key.code == KeyCode::PageDown || (ctrl && key.code == KeyCode::Char('v')) {
            self.scroll = (self.scroll + self.page).min(self.bottom());
            self.follow = self.scroll == self.bottom();
            return None;
        }
        if self.composing {
            if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')) {
                self.composing = false;
                return None;
            }
            if ctrl && key.code == KeyCode::Char('c') {
                if previous_c {
                    let body = self.draft.text();
                    if !self.connected || self.session.id.is_empty() {
                        self.status = "Bridge disconnected; draft retained".into();
                    } else if self.pending.is_some() {
                        self.status = "Waiting for send acknowledgement".into();
                    } else if !body.trim().is_empty() {
                        let request = self.request("send_message", json!({"body":body}));
                        self.pending = Some((self.next_request, body));
                        self.status = "Sending through inbox…".into();
                        return Some(request);
                    }
                } else {
                    self.ctrl_c = true;
                    self.status = "C-c C-c sends · C-x C-c detaches".into();
                }
                return None;
            }
            self.draft.key(key);
        } else {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return self.detach(),
                KeyCode::Char('c' | 'g') if ctrl => return self.detach(),
                KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                    self.follow = false;
                    self.scroll = self.scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                    self.scroll = (self.scroll + 1).min(self.bottom());
                }
                KeyCode::Enter | KeyCode::Char('i') => self.composing = true,
                _ => {}
            }
        }
        None
    }
}

// Wrap visible text without allowing ANSI/control bytes from tool output to
// become terminal commands. Native source data stays intact in the snapshot.
fn display_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![];
    for line in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            let n = Span::raw(ch.to_string()).width();
            if used + n > width && !row.is_empty() {
                lines.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += n;
        }
        lines.push(row);
    }
    lines
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(2),
        Constraint::Length(5),
        Constraint::Length(2),
    ])
    .split(frame.area());
    app.page = areas[1].height.saturating_sub(2).max(1) as usize;
    let width = areas[1].width.saturating_sub(2).max(1) as usize;
    if app.width != width {
        app.rebuild(width);
    }
    if app.follow {
        app.scroll = app.bottom();
    }
    let header = format!(
        "{} · {} · {}/{} · {} queued{}\n{}",
        app.session.title,
        app.session.profile,
        app.session.lifecycle,
        app.session.activity,
        app.session.pending,
        if app.connected {
            ""
        } else {
            " · DISCONNECTED"
        },
        app.session.workspace
    );
    frame.render_widget(
        Paragraph::new(display_text(&header)).style(Style::default().fg(Color::Cyan)),
        areas[0],
    );
    let title = format!(
        " Conversation · {}{} ",
        if app.follow { "following" } else { "backlog" },
        if app.earlier { " · M-< earlier" } else { "" }
    );
    let rows: Vec<Line> = app
        .rows
        .iter()
        .skip(app.scroll)
        .take(app.page)
        .map(|r| Line::styled(r.text.clone(), Style::default().fg(r.color)))
        .collect();
    frame.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(if app.composing {
                    Color::DarkGray
                } else {
                    Color::Cyan
                })),
        ),
        areas[1],
    );
    let composer = Block::default()
        .borders(Borders::ALL)
        .title(" Message · C-c C-c send · Enter newline ")
        .border_style(Style::default().fg(if app.composing {
            Color::Cyan
        } else {
            Color::DarkGray
        }));
    let input = composer.inner(areas[2]);
    let layout = app.draft.layout(input.width as usize);
    let (col, line) = layout.cursor;
    let top = line.saturating_sub(input.height.saturating_sub(1) as usize);
    let draft: Vec<Line> = layout
        .rows
        .into_iter()
        .skip(top)
        .take(input.height as usize)
        .map(Line::raw)
        .collect();
    frame.render_widget(Paragraph::new(draft).block(composer), areas[2]);
    let mut footer = format!(
        "{}\nTab transcript/composer · C-s search · M-x commands · C-x C-c detach",
        app.status
    );
    if let Some(overlay) = &app.overlay {
        footer = match overlay {
            Overlay::DiscardDraft => "Discard unsent draft and detach? y/n".into(),
            Overlay::ConfirmStop(run) => format!("Stop {run} and pause new work? y/n"),
            Overlay::Search(q) => format!(
                "Search: {} · C-s next · C-r previous · Enter done",
                q.text()
            ),
            Overlay::Commands(index) => format!(
                "M-x {}\n↑/↓ choose · Enter execute · Esc cancel",
                COMMANDS[*index]
            ),
        };
    }
    frame.render_widget(
        Paragraph::new(display_text(&footer)).style(Style::default().fg(Color::Cyan)),
        areas[3],
    );
    if app.composing && app.overlay.is_none() && input.width > 0 && input.height > 0 {
        frame.set_cursor_position((input.x + col as u16, input.y + (line - top) as u16));
    }
}

pub fn run(height: u16) -> Result<()> {
    let interrupted = termination_flag()?;
    let (sender, receiver) = mpsc::sync_channel(4);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let parsed = line
                .map_err(anyhow::Error::from)
                .and_then(|line| parse_input(&line));
            if sender.send(parsed).is_err() {
                break;
            }
        }
    });
    let mut app = App::default();
    let mut terminal = InlineTerminal::enter(height.max(11))?;
    terminal.enable_bracketed_paste()?;
    let mut stdout = io::stdout().lock();
    loop {
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        loop {
            match receiver.try_recv() {
                Ok(Ok(input)) => app.apply(input)?,
                Ok(Err(error)) => {
                    app.status = format!("Invalid bridge frame: {error}");
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.connected = false;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        terminal.draw(|frame| draw(frame, &mut app))?;
        if let Some(intent) = app.viewed_intent() {
            serde_json::to_writer(&mut stdout, &intent)?;
            writeln!(stdout)?;
            stdout.flush()?;
        }
        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if let Some(step) = ResizeStep::from_key(key, app.ctrl_x) {
                    app.ctrl_x = false;
                    terminal.resize_by(step, 11)?;
                    continue;
                }
                if let Some(intent) = app.key(key) {
                    serde_json::to_writer(&mut stdout, &intent)?;
                    writeln!(stdout)?;
                    stdout.flush()?;
                    if intent["intent"] == "dismiss" {
                        break;
                    }
                }
            }
            Event::Paste(text) if app.composing && app.overlay.is_none() => app.draft.insert(&text),
            _ => {}
        }
    }
    drop(terminal);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn snapshot(entries: Vec<Value>) -> Input {
        parse_input(&json!({"schema_version":1,"type":"snapshot","session":{"id":"s","title":"Gusgus","workspace":"/repo","profile":"jcode","lifecycle":"open","activity":"running","run_id":"r","pending":0},"entries":entries,"has_earlier":true,"window":400}).to_string()).unwrap()
    }
    fn composer_row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (1..buffer.area.width - 1)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }
    #[test]
    fn composer_wraps_and_reflows_without_changing_the_message() {
        let mut app = App::default();
        app.apply(snapshot(vec![])).unwrap();
        app.draft.insert("abcdefghijklmnop");
        let mut terminal = Terminal::new(TestBackend::new(14, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(composer_row(&terminal, 10), "abcdefghijkl");
        assert_eq!(composer_row(&terminal, 11), "mnop        ");
        assert_eq!(terminal.get_cursor_position().unwrap(), (5, 11).into());

        terminal.backend_mut().resize(10, 16);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(composer_row(&terminal, 10), "abcdefgh");
        assert_eq!(composer_row(&terminal, 11), "ijklmnop");
        assert_eq!(terminal.get_cursor_position().unwrap(), (1, 12).into());
        app.key(key('c'));
        assert_eq!(app.key(key('c')).unwrap()["body"], "abcdefghijklmnop");
    }
    #[test]
    fn composer_scrolls_wrapped_rows_to_keep_the_cursor_visible() {
        let mut app = App::default();
        app.draft.insert("abcdefghijklmnopqrstuvwxyz0123");
        let mut terminal = Terminal::new(TestBackend::new(10, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(composer_row(&terminal, 10), "ijklmnop");
        assert_eq!(composer_row(&terminal, 11), "qrstuvwx");
        assert_eq!(composer_row(&terminal, 12), "yz0123  ");
        assert_eq!(terminal.get_cursor_position().unwrap(), (7, 12).into());

        app.key(key('a'));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(composer_row(&terminal, 10), "abcdefgh");
        assert_eq!(terminal.get_cursor_position().unwrap(), (1, 10).into());
        app.key(key('e'));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap(), (7, 12).into());
    }
    #[test]
    fn composer_wraps_unicode_graphemes_and_preserves_hard_newlines() {
        let mut app = App::default();
        let text = "abcdef日本語\ne\u{301} 👩‍💻 z";
        app.draft.insert(text);
        let mut terminal = Terminal::new(TestBackend::new(10, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(7, 10)].symbol(), "日");
        assert_eq!(buffer[(1, 11)].symbol(), "本");
        assert_eq!(buffer[(3, 11)].symbol(), "語");
        assert_eq!(buffer[(1, 12)].symbol(), "e\u{301}");
        assert_eq!(buffer[(3, 12)].symbol(), "👩‍💻");
        assert_eq!(buffer[(6, 12)].symbol(), "z");
        assert_eq!(terminal.get_cursor_position().unwrap(), (7, 12).into());
        assert_eq!(app.draft.text(), text);
    }
    #[test]
    fn draft_survives_updates_and_failed_send() {
        let mut a = App::default();
        a.apply(snapshot(vec![])).unwrap();
        a.draft.insert("literal $()\n日本語");
        assert!(a.key(key('c')).is_none());
        let send = a.key(key('c')).unwrap();
        assert_eq!(send["body"], "literal $()\n日本語");
        a.apply(snapshot(vec![])).unwrap();
        a.apply(Input::Ack {
            schema_version: 1,
            request_id: 1,
            ok: false,
            message: "paused".into(),
        })
        .unwrap();
        assert_eq!(a.draft.text(), "literal $()\n日本語");
        a.key(key('c'));
        a.key(key('c'));
        a.apply(Input::Ack {
            schema_version: 1,
            request_id: 2,
            ok: true,
            message: "sent".into(),
        })
        .unwrap();
        assert!(a.draft.text().is_empty());
    }
    #[test]
    fn stop_pins_displayed_run() {
        let mut a = App::default();
        a.apply(snapshot(vec![])).unwrap();
        a.overlay = Some(Overlay::ConfirmStop("r".into()));
        a.session.run_id = "replacement".into();
        assert_eq!(
            a.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .unwrap()["run_id"],
            "r"
        );
    }
    #[test]
    fn detach_is_never_a_stop() {
        let mut a = App::default();
        a.apply(snapshot(vec![])).unwrap();
        a.key(key('x'));
        assert_eq!(a.key(key('c')).unwrap()["intent"], "dismiss");
        a.draft.insert("unsent");
        assert!(a.detach().is_none());
        assert!(matches!(a.overlay, Some(Overlay::DiscardDraft)));
    }
    #[test]
    fn older_snapshot_preserves_scroll_anchor() {
        let mut a = App::default();
        let e =
            |id: &str| json!({"id":id,"kind":"message","title":id,"text":"many\nlines\nof\ntext"});
        a.apply(snapshot(vec![e("a"), e("b"), e("c")])).unwrap();
        a.follow = false;
        a.scroll = 3;
        let anchor = a.rows[3].id.clone();
        a.apply(snapshot(vec![e("older"), e("a"), e("b"), e("c")]))
            .unwrap();
        assert_eq!(a.rows[a.scroll].id, anchor);
    }
    #[test]
    fn unicode_editing_and_kill_yank() {
        let mut d = Draft::default();
        d.insert("日本語\nnext");
        d.key(key('a'));
        d.key(key('k'));
        assert_eq!(d.text(), "日本語\n");
        d.key(key('y'));
        assert_eq!(d.text(), "日本語\nnext");
        d.key(key('p'));
        d.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(d.text(), "日本\nnext");
    }
    #[test]
    fn displayed_fields_keep_text_without_terminal_controls() {
        let raw = "日本語\n\x1b]52;c;clipboard\x07\rtext";
        assert_eq!(display_text(raw), "日本語\n ]52;c;clipboard  text");
        let mut draft = Draft::default();
        draft.insert(raw);
        let _ = display_text(&draft.text());
        assert_eq!(
            draft.text(),
            raw,
            "presentation does not alter the sent body"
        );
    }
    #[test]
    fn schema_and_session_are_pinned() {
        assert!(
            parse_input(
                r#"{"schema_version":2,"type":"ack","request_id":1,"ok":true,"message":"x"}"#
            )
            .is_err()
        );
        let mut a = App::default();
        a.apply(snapshot(vec![])).unwrap();
        let mut next = snapshot(vec![]);
        if let Input::Snapshot { session, .. } = &mut next {
            session.id = "other".into();
        }
        assert!(a.apply(next).is_err());
    }
}
