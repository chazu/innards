use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tui_input::Input;
use tui_input::backend::crossterm::to_input_request;

use crate::inline_terminal::{InlineTerminal, termination_flag};

const DEFAULT_HEIGHT: u16 = 20;
const MIN_PICKER_HEIGHT: u16 = 10;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Candidate {
    pub schema_version: u8,
    pub id: String,
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub detail: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Candidate {
    fn validate(&self, record: usize) -> Result<()> {
        if self.schema_version != 1 {
            return Err(anyhow!(
                "candidate {record} uses unsupported schema version {}",
                self.schema_version
            ));
        }
        if self.id.is_empty() || self.label.is_empty() || self.path.as_os_str().is_empty() {
            return Err(anyhow!(
                "candidate {record} requires non-empty id, label, and path"
            ));
        }
        if self.line == 0 || self.column == 0 {
            return Err(anyhow!("candidate {record} positions must be one-based"));
        }
        Ok(())
    }
}

pub fn read_json_lines(reader: impl BufRead) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read candidate {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let candidate: Candidate = serde_json::from_str(&line)
            .with_context(|| format!("invalid candidate JSON on line {}", index + 1))?;
        candidate.validate(index + 1)?;
        candidates.push(candidate);
    }
    Ok(candidates)
}

pub trait Provider {
    fn search(&self, query: &str) -> Vec<Candidate>;
}

#[derive(Clone, Debug)]
pub struct StaticProvider {
    candidates: Vec<Candidate>,
}

impl StaticProvider {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self { candidates }
    }
}

impl Provider for StaticProvider {
    fn search(&self, query: &str) -> Vec<Candidate> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        self.candidates
            .iter()
            .filter(|candidate| {
                let searchable = format!(
                    "{} {} {} {} {}",
                    candidate.id,
                    candidate.label,
                    candidate.kind,
                    candidate.detail,
                    candidate.path.display()
                )
                .to_lowercase();
                terms.iter().all(|term| searchable.contains(term))
            })
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub height: u16,
    pub title: String,
    pub initial_query: String,
}

impl Config {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            height: DEFAULT_HEIGHT,
            title: "inpick".to_string(),
            initial_query: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Selected,
    Cancelled,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Selected => 0,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PickerResult {
    pub schema_version: u8,
    pub outcome: Outcome,
    pub selection: Option<Candidate>,
}

pub fn write_result_json(result: &PickerResult) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, result)?;
    writeln!(stdout)?;
    Ok(())
}

pub fn run_with(provider: &impl Provider, config: Config) -> Result<PickerResult> {
    let interrupted = termination_flag()?;
    let mut app = App::new(provider, config);
    let mut terminal = InlineTerminal::enter(app.config.height.max(MIN_PICKER_HEIGHT))?;

    loop {
        if interrupted.load(Ordering::Relaxed) {
            drop(terminal);
            return Ok(app.result(Outcome::Cancelled, None));
        }
        terminal.draw(|frame| draw(frame, &app))?;

        let ready = match event::poll(Duration::from_millis(80)) {
            Ok(ready) => ready,
            Err(_) if interrupted.load(Ordering::Relaxed) => {
                drop(terminal);
                return Ok(app.result(Outcome::Cancelled, None));
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
        match handle_key(&mut app, provider, key) {
            Action::Continue => {}
            Action::Select(candidate) => {
                drop(terminal);
                return Ok(app.result(Outcome::Selected, Some(candidate)));
            }
            Action::Cancel => {
                drop(terminal);
                return Ok(app.result(Outcome::Cancelled, None));
            }
        }
    }
}

struct App {
    config: Config,
    input: Input,
    matches: Vec<Candidate>,
    selected: usize,
    preview_scroll: isize,
}

impl App {
    fn new(provider: &impl Provider, config: Config) -> Self {
        let input = Input::from(config.initial_query.clone());
        let matches = provider.search(input.value());
        Self {
            config,
            input,
            matches,
            selected: 0,
            preview_scroll: 0,
        }
    }

    fn selected(&self) -> Option<&Candidate> {
        self.matches.get(self.selected)
    }

    fn refresh(&mut self, provider: &impl Provider) {
        self.matches = provider.search(self.input.value());
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.preview_scroll = 0;
    }

    fn result(&self, outcome: Outcome, selection: Option<Candidate>) -> PickerResult {
        PickerResult {
            schema_version: 1,
            outcome,
            selection,
        }
    }
}

enum Action {
    Continue,
    Select(Candidate),
    Cancel,
}

fn handle_key(app: &mut App, provider: &impl Provider, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => return Action::Cancel,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Action::Cancel;
        }
        KeyCode::Enter => {
            if let Some(candidate) = app.selected().cloned() {
                return Action::Select(candidate);
            }
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.preview_scroll = app.preview_scroll.saturating_sub(1);
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.preview_scroll = app.preview_scroll.saturating_add(1);
        }
        KeyCode::Up => {
            app.selected = app.selected.saturating_sub(1);
            app.preview_scroll = 0;
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.selected = app.selected.saturating_sub(1);
            app.preview_scroll = 0;
        }
        KeyCode::Down => {
            if !app.matches.is_empty() {
                app.selected = (app.selected + 1).min(app.matches.len() - 1);
                app.preview_scroll = 0;
            }
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.matches.is_empty() {
                app.selected = (app.selected + 1).min(app.matches.len() - 1);
                app.preview_scroll = 0;
            }
        }
        KeyCode::PageUp => {
            app.selected = app.selected.saturating_sub(10);
            app.preview_scroll = 0;
        }
        KeyCode::PageDown => {
            if !app.matches.is_empty() {
                app.selected = (app.selected + 10).min(app.matches.len() - 1);
                app.preview_scroll = 0;
            }
        }
        _ => {
            let event = Event::Key(key);
            if let Some(request) = to_input_request(&event)
                && app.input.handle(request).is_some()
            {
                app.refresh(provider);
            }
        }
    }
    Action::Continue
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    frame.render_widget(Clear, frame.area());
    let [query_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [list_area, preview_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body_area);

    let query = Paragraph::new(app.input.value()).block(
        Block::default()
            .title(app.config.title.as_str())
            .borders(Borders::ALL),
    );
    frame.render_widget(query, query_area);
    let cursor = app
        .input
        .visual_cursor()
        .min(query_area.width.saturating_sub(2) as usize) as u16;
    frame.set_cursor_position((query_area.x + 1 + cursor, query_area.y + 1));

    let items: Vec<ListItem<'_>> = app.matches.iter().map(candidate_item).collect();
    let list = List::new(items)
        .block(Block::default().title(" Candidates ").borders(Borders::ALL))
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default();
    if !app.matches.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, list_area, &mut state);

    draw_preview(frame, preview_area, app);
    let status = format!(
        "{} match(es) | enter select | esc cancel | arrows move | shift-arrows preview",
        app.matches.len()
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        status_area,
    );
}

fn candidate_item(candidate: &Candidate) -> ListItem<'_> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<12}", candidate.kind),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(candidate.label.as_str(), Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(
            format!("{}:{}", candidate.path.display(), candidate.line),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            candidate.detail.as_str(),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(candidate) = app.selected() else {
        frame.render_widget(
            Paragraph::new("No candidate selected")
                .block(Block::default().title(" Preview ").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let path = resolve_path(&app.config.root, &candidate.path);
    let title = format!(" Preview {}:{} ", candidate.path.display(), candidate.line);
    let lines = preview_lines(&path, candidate.line, area, app.preview_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn preview_lines(path: &Path, line: usize, area: Rect, scroll: isize) -> Vec<Line<'static>> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return vec![Line::from(format!("Unable to read {}", path.display()))];
    };
    let source: Vec<&str> = contents.lines().collect();
    if source.is_empty() {
        return vec![Line::from(format!("{} is empty", path.display()))];
    }
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let target = line.saturating_sub(1) as isize;
    let max_start = source.len().saturating_sub(visible) as isize;
    let start = (target - 2 + scroll).clamp(0, max_start).max(0) as usize;
    let end = (start + visible).min(source.len());

    source[start..end]
        .iter()
        .enumerate()
        .map(|(offset, text)| {
            let line_number = start + offset + 1;
            let style = if line_number == line {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(vec![
                Span::styled(
                    format!("{line_number:>5} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled((*text).to_string(), style),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn candidate(id: &str, label: &str) -> Candidate {
        Candidate {
            schema_version: 1,
            id: id.to_string(),
            path: PathBuf::from("trash/Kube/Pod.trash"),
            line: 12,
            column: 3,
            label: label.to_string(),
            kind: "method".to_string(),
            detail: "instance method".to_string(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn parses_namespaced_and_keyword_selector_records() {
        let record = serde_json::to_string(&candidate(
            "Kube::Pod>>fromJson:cluster:",
            "Kube::Pod>>fromJson:cluster:",
        ))
        .unwrap();

        let parsed = read_json_lines(Cursor::new(format!("{record}\n"))).unwrap();

        assert_eq!(parsed[0].id, "Kube::Pod>>fromJson:cluster:");
        assert_eq!(parsed[0].line, 12);
    }

    #[test]
    fn static_provider_matches_all_query_terms() {
        let provider = StaticProvider::new(vec![
            candidate("Kube::Pod>>ready", "Kube::Pod>>ready"),
            candidate("Array>>at:put:", "Array>>at:put:"),
        ]);

        let matches = provider.search("array put:");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "Array>>at:put:");
    }

    #[test]
    fn parser_rejects_zero_based_positions() {
        let record = r#"{"schema_version":1,"id":"x","path":"x.trash","line":0,"column":1,"label":"x","kind":"class"}"#;

        let error = read_json_lines(Cursor::new(record)).unwrap_err();

        assert!(error.to_string().contains("one-based"));
    }
}
