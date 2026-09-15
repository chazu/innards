use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tui_input::Input;
use tui_input::backend::crossterm::to_input_request;

use crate::inline_terminal::{InlineTerminal, termination_flag};
use crate::preview::PreviewCache;
use crate::redraw::{IDLE_POLL, Redraw};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<CandidateDisplay>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Optional compact presentation for records whose path is only a preview source.
/// Identity and selection still use the original candidate fields.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CandidateDisplay {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub preview_title: String,
    #[serde(default)]
    pub search_text: String,
    /// Named values rendered as a table in the candidate pane.  A picker
    /// unions names across its candidates to form the header row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<CandidateColumn>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CandidateColumn {
    pub name: String,
    pub value: String,
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

    /// Replace a candidate's display data when it is enriched by a preview
    /// hook. Providers which do not cache searchable data can ignore this.
    fn update_display(&self, _id: &str, _display: CandidateDisplay) {}
}

#[derive(Clone, Debug)]
pub struct StaticProvider {
    candidates: RefCell<Vec<Candidate>>,
    /// Lower-cased searchable text per candidate, built once at load so a
    /// keystroke only runs `contains` over each entry.
    haystacks: RefCell<Vec<String>>,
}

impl StaticProvider {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        let haystacks = candidates.iter().map(search_haystack).collect();
        Self {
            candidates: RefCell::new(candidates),
            haystacks: RefCell::new(haystacks),
        }
    }
}

/// Every field a query term may match, joined in a fixed order and lower-cased.
fn search_haystack(candidate: &Candidate) -> String {
    format!(
        "{} {} {} {} {} {} {} {}",
        candidate.id,
        candidate.label,
        candidate.kind,
        candidate.detail,
        candidate.path.display(),
        candidate.display.as_ref().map_or("", |d| d.prefix.as_str()),
        candidate
            .display
            .as_ref()
            .map_or("", |d| d.search_text.as_str()),
        candidate
            .display
            .as_ref()
            .map(|d| {
                d.columns
                    .iter()
                    .map(|column| format!("{} {}", column.name, column.value))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    )
    .to_lowercase()
}

impl Provider for StaticProvider {
    fn search(&self, query: &str) -> Vec<Candidate> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        self.candidates
            .borrow()
            .iter()
            .zip(self.haystacks.borrow().iter())
            .filter(|(_, haystack)| terms.iter().all(|term| haystack.contains(term.as_str())))
            .map(|(candidate, _)| candidate.clone())
            .collect()
    }

    fn update_display(&self, id: &str, display: CandidateDisplay) {
        let mut candidates = self.candidates.borrow_mut();
        let Some((index, candidate)) = candidates
            .iter_mut()
            .enumerate()
            .find(|(_, candidate)| candidate.id == id)
        else {
            return;
        };
        candidate.display = Some(display);
        self.haystacks.borrow_mut()[index] = search_haystack(candidate);
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub height: u16,
    pub title: String,
    pub initial_query: String,
    pub ctrl_d_action: Option<String>,
    pub preview_hook: Option<PathBuf>,
}

impl Config {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            height: DEFAULT_HEIGHT,
            title: "inpick".to_string(),
            initial_query: String::new(),
            ctrl_d_action: None,
            preview_hook: None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
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
    let mut redraw = Redraw::new();

    loop {
        if interrupted.load(Ordering::Relaxed) {
            drop(terminal);
            return Ok(app.result(Outcome::Cancelled, None));
        }
        if redraw.take() {
            let mut preview_visible = false;
            terminal.draw(|frame| preview_visible = draw(frame, &mut app))?;
            if preview_visible && app.notify_preview(provider)? {
                // The hook changed a row; show it before waiting for input.
                redraw.request();
                continue;
            }
        }

        let ready = match event::poll(IDLE_POLL) {
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
        let event = event::read()?;
        redraw.request();
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if terminal.handle_resize_key(key, MIN_PICKER_HEIGHT)? {
            continue;
        }
        match handle_key(&mut app, provider, key) {
            Action::Continue => {}
            Action::Select(candidate, action) => {
                drop(terminal);
                let mut result = app.result(Outcome::Selected, Some(candidate));
                result.action = action;
                return Ok(result);
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
    /// Union of property column names across the matches, or `None` when the
    /// plain list layout applies. Recomputed whenever the matches change.
    columns: Option<Vec<String>>,
    selected: usize,
    preview_scroll: isize,
    previewed: HashSet<String>,
    preview_displays: BTreeMap<String, CandidateDisplay>,
    preview: PreviewCache,
}

impl App {
    fn new(provider: &impl Provider, config: Config) -> Self {
        let input = Input::from(config.initial_query.clone());
        let matches = provider.search(input.value());
        let columns = table_columns(&matches);
        Self {
            config,
            input,
            matches,
            columns,
            selected: 0,
            preview_scroll: 0,
            previewed: HashSet::new(),
            preview_displays: BTreeMap::new(),
            preview: PreviewCache::new(),
        }
    }

    fn selected(&self) -> Option<&Candidate> {
        self.matches.get(self.selected)
    }

    fn refresh(&mut self, provider: &impl Provider) {
        self.matches = provider.search(self.input.value());
        for candidate in &mut self.matches {
            if let Some(display) = self.preview_displays.get(&candidate.id) {
                candidate.display = Some(display.clone());
            }
        }
        self.columns = table_columns(&self.matches);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.preview_scroll = 0;
    }

    /// Run the preview hook once for the selected candidate. Returns whether
    /// the hook changed the candidate's display, which needs a fresh frame.
    fn notify_preview(&mut self, provider: &impl Provider) -> Result<bool> {
        let Some(hook) = &self.config.preview_hook else {
            return Ok(false);
        };
        let Some(candidate) = self.selected() else {
            return Ok(false);
        };
        if self.previewed.contains(&candidate.id) {
            return Ok(false);
        }
        let id = candidate.id.clone();
        let mut child = Command::new(hook)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start preview hook {}", hook.display()))?;
        let mut input = serde_json::to_vec(candidate)?;
        input.push(b'\n');
        let sent = child.stdin.take().expect("piped stdin").write_all(&input);
        let output = child
            .wait_with_output()
            .context("failed to wait for preview hook")?;
        sent.context("failed to send preview to hook")?;
        if !output.status.success() {
            return Err(anyhow!(
                "preview hook failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let mut updated = false;
        if !output.stdout.iter().all(u8::is_ascii_whitespace) {
            let display: CandidateDisplay = serde_json::from_slice(&output.stdout)
                .context("preview hook returned an invalid display object")?;
            self.preview_displays.insert(id.clone(), display.clone());
            provider.update_display(&id, display.clone());
            for candidate in &mut self.matches {
                if candidate.id == id {
                    candidate.display = Some(display.clone());
                }
            }
            self.columns = table_columns(&self.matches);
            updated = true;
        }
        self.previewed.insert(id);
        Ok(updated)
    }

    fn result(&self, outcome: Outcome, selection: Option<Candidate>) -> PickerResult {
        PickerResult {
            schema_version: 1,
            outcome,
            selection,
            action: None,
        }
    }
}

enum Action {
    Continue,
    Select(Candidate, Option<String>),
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
                return Action::Select(candidate, None);
            }
        }
        KeyCode::Char('d')
            if key.modifiers == KeyModifiers::CONTROL && app.config.ctrl_d_action.is_some() =>
        {
            if let Some(candidate) = app.selected().cloned() {
                return Action::Select(candidate, app.config.ctrl_d_action.clone());
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

fn draw(frame: &mut Frame<'_>, app: &mut App) -> bool {
    frame.render_widget(Clear, frame.area());
    let [query_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let list_percent = if app.selected().is_some_and(|c| c.display.is_some()) {
        40
    } else {
        55
    };
    let [list_area, preview_area] = Layout::vertical([
        Constraint::Percentage(list_percent),
        Constraint::Percentage(100 - list_percent),
    ])
    .areas(body_area);

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

    draw_candidates(frame, list_area, app);
    let preview_visible = draw_preview(frame, preview_area, app);
    let action_hint = app
        .config
        .ctrl_d_action
        .as_ref()
        .map(|action| format!(" | ctrl-d {action}"))
        .unwrap_or_default();
    let status = format!(
        "{} match(es) | enter select{} | esc cancel | arrows move | shift-arrows preview",
        app.matches.len(),
        action_hint
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        status_area,
    );
    preview_visible
}

/// Only the rows that fit are built. A fresh list state scrolls so the
/// selection sits on the bottom row once it passes the visible height, and the
/// window below reproduces that placement without allocating every match.
fn draw_candidates(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().title(" Candidates ").borders(Borders::ALL);
    let highlight = Style::default()
        .bg(Color::DarkGray)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let inner_height = usize::from(block.inner(area).height);
    if let Some(columns) = &app.columns {
        let headers = std::iter::once("object".to_string())
            .chain(columns.iter().cloned())
            .collect::<Vec<_>>();
        let visible = inner_height.saturating_sub(1);
        let first = visible_window_start(app.selected, visible);
        let rows = app
            .matches
            .iter()
            .skip(first)
            .take(visible)
            .map(|candidate| {
                Row::new(
                    std::iter::once(candidate.label.clone())
                        .chain(columns.iter().map(|name| candidate_column(candidate, name)))
                        .collect::<Vec<_>>(),
                )
            });
        let count = headers.len() as u16;
        let widths = (0..headers.len())
            .map(|_| Constraint::Percentage(100 / count))
            .collect::<Vec<_>>();
        let table = Table::new(rows, widths)
            .header(
                Row::new(headers).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .block(block)
            .column_spacing(1)
            .row_highlight_style(highlight)
            .highlight_symbol("> ");
        let mut state = TableState::default();
        if !app.matches.is_empty() && visible > 0 {
            state.select(Some(app.selected - first));
        }
        frame.render_stateful_widget(table, area, &mut state);
    } else {
        let first = visible_window_start(app.selected, inner_height);
        let items: Vec<ListItem<'_>> = app
            .matches
            .iter()
            .skip(first)
            .take(inner_height)
            .map(candidate_item)
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_style(highlight);
        let mut state = ListState::default();
        if !app.matches.is_empty() && inner_height > 0 {
            state.select(Some(app.selected - first));
        }
        frame.render_stateful_widget(list, area, &mut state);
    }
}

/// First row of the window a fresh list state shows for `selected`.
fn visible_window_start(selected: usize, visible: usize) -> usize {
    if visible == 0 {
        0
    } else {
        selected.saturating_sub(visible - 1)
    }
}

fn table_columns(candidates: &[Candidate]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for candidate in candidates {
        for column in candidate.display.as_ref()?.columns.iter() {
            if !column.name.is_empty() && !names.contains(&column.name) {
                names.push(column.name.clone());
            }
        }
    }
    (!names.is_empty()).then_some(names)
}

fn candidate_column(candidate: &Candidate, name: &str) -> String {
    candidate
        .display
        .as_ref()
        .and_then(|display| display.columns.iter().find(|column| column.name == name))
        .map_or_else(String::new, |column| column.value.clone())
}

fn candidate_item(candidate: &Candidate) -> ListItem<'_> {
    if let Some(display) = &candidate.display {
        return ListItem::new(Line::from(vec![
            Span::styled(display.prefix.as_str(), Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(candidate.label.as_str(), Style::default().fg(Color::White)),
        ]));
    }
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<12} ", candidate.kind),
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

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> bool {
    let Some(candidate) = app.selected() else {
        frame.render_widget(
            Paragraph::new("No candidate selected")
                .block(Block::default().title(" Preview ").borders(Borders::ALL)),
            area,
        );
        return false;
    };
    let path = resolve_path(&app.config.root, &candidate.path);
    let line = candidate.line;
    let title = match &candidate.display {
        Some(display) => format!(
            " {} ",
            if display.preview_title.is_empty() {
                "Preview"
            } else {
                &display.preview_title
            }
        ),
        None => format!(" Preview {}:{} ", candidate.path.display(), candidate.line),
    };
    let scroll = app.preview_scroll;
    let (lines, readable) = preview_lines(app.preview.lines(&path), &path, line, area, scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
    readable && area.height > 2 && area.width > 8
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn preview_lines(
    source: Option<&[String]>,
    path: &Path,
    line: usize,
    area: Rect,
    scroll: isize,
) -> (Vec<Line<'static>>, bool) {
    let Some(source) = source else {
        return (
            vec![Line::from(format!("Unable to read {}", path.display()))],
            false,
        );
    };
    if source.is_empty() {
        return (
            vec![Line::from(format!("{} is empty", path.display()))],
            true,
        );
    }
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let target = line.saturating_sub(1) as isize;
    let max_start = source.len().saturating_sub(visible) as isize;
    let start = (target - 2 + scroll).clamp(0, max_start).max(0) as usize;
    let end = (start + visible).min(source.len());

    let lines = source[start..end]
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
                Span::styled(text.clone(), style),
            ])
        })
        .collect();
    (lines, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(test_name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "innards-picker-unit-{test_name}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
            display: None,
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

    #[test]
    fn compact_display_keeps_identity_and_hidden_text_searchable() {
        let record = r#"{"schema_version":1,"id":"message_123","path":"message_123.txt","line":1,"column":1,"label":"Tests pass","kind":"result","display":{"prefix":"● Gusgus  14:32","preview_title":"Message","search_text":"session:agentsession_456 second paragraph"}}"#;
        let candidates = read_json_lines(Cursor::new(record)).unwrap();
        let provider = StaticProvider::new(candidates.clone());
        assert_eq!(provider.search("Gusgus paragraph")[0].id, "message_123");
        assert_eq!(provider.search("agentsession_456")[0], candidates[0]);
        assert!(provider.search("unrelated").is_empty());
        let encoded = serde_json::to_value(&candidates[0]).unwrap();
        assert_eq!(encoded["display"]["preview_title"], "Message");
    }

    #[test]
    fn compact_display_hides_file_machinery_and_gives_preview_more_space() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut message = candidate("message_123", "Tests pass");
        message.path = PathBuf::from("message_123.txt");
        message.kind = "result".into();
        message.display = Some(CandidateDisplay {
            prefix: "● Gusgus  14:32".into(),
            preview_title: "Message".into(),
            search_text: String::new(),
            columns: vec![],
        });
        let provider = StaticProvider::new(vec![message]);
        let mut app = App::new(&provider, Config::new("."));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &mut app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(4).contains("● Gusgus  14:32  Tests pass"));
        assert!(!row(4).contains("message_123.txt"));
        assert!(row(9).contains("Message"));
        assert!(!row(9).contains("message_123.txt"));
    }

    #[test]
    fn property_columns_render_a_header_and_aligned_object_rows() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut first = candidate("counter-1", "Counter 00000001");
        first.display = Some(CandidateDisplay {
            prefix: String::new(),
            preview_title: String::new(),
            search_text: String::new(),
            columns: vec![
                CandidateColumn {
                    name: "value".into(),
                    value: "0".into(),
                },
                CandidateColumn {
                    name: "step".into(),
                    value: "1".into(),
                },
            ],
        });
        let mut second = candidate("counter-2", "Counter 00000002");
        second.display = Some(CandidateDisplay {
            prefix: String::new(),
            preview_title: String::new(),
            search_text: String::new(),
            columns: vec![CandidateColumn {
                name: "value".into(),
                value: "37".into(),
            }],
        });
        let provider = StaticProvider::new(vec![first, second]);
        let mut app = App::new(&provider, Config::new("."));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &mut app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(4).contains("object"));
        assert!(row(4).contains("value"));
        assert!(row(4).contains("step"));
        assert!(row(5).contains("Counter 00000001"));
        assert!(row(5).contains("0"));
        assert!(row(6).contains("Counter 00000002"));
        assert!(table_columns(&app.matches).is_some());
    }

    #[test]
    fn control_d_action_requires_opt_in_and_a_match() {
        let provider = StaticProvider::new(vec![candidate("message-1", "hello")]);
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let mut app = App::new(&provider, Config::new("."));
        assert!(matches!(
            handle_key(&mut app, &provider, key),
            Action::Continue
        ));
        app.config.ctrl_d_action = Some("archive".into());
        app.input = Input::from("no matches".to_string());
        app.refresh(&provider);
        assert!(matches!(
            handle_key(&mut app, &provider, key),
            Action::Continue
        ));
    }

    #[test]
    fn enter_does_not_request_the_optional_action() {
        let provider = StaticProvider::new(vec![candidate("message-1", "hello")]);
        let mut config = Config::new(".");
        config.ctrl_d_action = Some("archive".into());
        let mut app = App::new(&provider, config);
        assert!(matches!(
            handle_key(
                &mut app,
                &provider,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Action::Select(_, None)
        ));
        let result = app.result(Outcome::Cancelled, None);
        assert!(
            serde_json::to_value(result)
                .unwrap()
                .get("action")
                .is_none()
        );
    }

    #[test]
    fn search_haystack_joins_every_field_lowercased_in_a_fixed_order() {
        let mut item = candidate("Kube::Pod>>ready", "Kube::Pod>>ready");
        item.display = Some(CandidateDisplay {
            prefix: "● Gusgus".into(),
            preview_title: "Message".into(),
            search_text: "Second Paragraph".into(),
            columns: vec![CandidateColumn {
                name: "Value".into(),
                value: "37".into(),
            }],
        });
        assert_eq!(
            search_haystack(&item),
            "kube::pod>>ready kube::pod>>ready method instance method trash/kube/pod.trash ● gusgus second paragraph value 37"
        );
        assert_eq!(
            search_haystack(&candidate("x", "X")),
            "x x method instance method trash/kube/pod.trash   "
        );
    }

    #[test]
    fn search_matches_property_columns_case_insensitively() {
        let mut item = candidate("counter-1", "Counter 00000001");
        item.display = Some(CandidateDisplay {
            prefix: String::new(),
            preview_title: String::new(),
            search_text: String::new(),
            columns: vec![CandidateColumn {
                name: "value".into(),
                value: "37".into(),
            }],
        });
        let provider = StaticProvider::new(vec![item, candidate("other", "Other")]);

        assert_eq!(provider.search("VALUE 37")[0].id, "counter-1");
        assert_eq!(provider.search("counter 37").len(), 1);
        assert!(provider.search("value 38").is_empty());
        assert_eq!(provider.search("").len(), 2);
    }

    #[test]
    fn preview_hook_display_data_remains_searchable_after_refresh() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = ScratchDir::new("preview-hook-search-cache");
        let hook = scratch.join("preview-hook.sh");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf '%s\\n' '{\"prefix\":\"\",\"preview_title\":\"\",\"search_text\":\"preview-hook-needle\",\"columns\":[{\"name\":\"hook_property\",\"value\":\"property-needle\"}]}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let provider = StaticProvider::new(vec![candidate("hooked", "original")]);
        let mut config = Config::new(".");
        config.preview_hook = Some(hook);
        let mut app = App::new(&provider, config);

        assert!(app.notify_preview(&provider).unwrap());
        app.input = Input::from("preview-hook-needle hook_property property-needle".to_string());
        app.refresh(&provider);

        assert_eq!(app.matches.len(), 1);
        assert_eq!(app.matches[0].id, "hooked");
    }

    #[test]
    fn visible_window_keeps_the_selection_on_the_bottom_row_once_it_scrolls() {
        assert_eq!(visible_window_start(0, 5), 0);
        assert_eq!(visible_window_start(4, 5), 0);
        assert_eq!(visible_window_start(5, 5), 1);
        assert_eq!(visible_window_start(29, 5), 25);
        assert_eq!(visible_window_start(7, 0), 0);
    }

    #[test]
    fn long_candidate_lists_build_only_the_visible_rows() {
        use ratatui::{Terminal, backend::TestBackend};
        let items = (0..40)
            .map(|index| candidate(&format!("id-{index:03}"), &format!("candidate-{index:03}")))
            .collect();
        let provider = StaticProvider::new(items);
        let mut app = App::new(&provider, Config::new("."));
        app.selected = 30;
        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &mut app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..70).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        let rows: Vec<String> = (0..20).map(row).collect();
        let selected = rows
            .iter()
            .position(|text| text.contains("> "))
            .expect("selected row is rendered");

        assert!(rows[selected].contains("candidate-030"));
        assert!(
            rows[selected + 1].starts_with('└'),
            "the selection sits on the last list row: {:?}",
            rows[selected + 1]
        );
        assert!(rows[selected - 1].contains("candidate-029"));
        assert!(!rows.iter().any(|text| text.contains("candidate-000")));
    }

    #[test]
    fn preview_source_is_read_once_across_redraws() {
        use ratatui::{Terminal, backend::TestBackend};
        let scratch = ScratchDir::new("preview-cache");
        let source = scratch.join("Pod.trash");
        std::fs::write(&source, "package: Kube\nPod subclass: Object\n").unwrap();
        let mut item = candidate("Kube::Pod", "Kube::Pod");
        item.path = source;
        item.line = 2;
        let provider = StaticProvider::new(vec![item]);
        let mut app = App::new(&provider, Config::new("."));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        for _ in 0..3 {
            terminal
                .draw(|frame| {
                    draw(frame, &mut app);
                })
                .unwrap();
        }

        assert_eq!(app.preview.reads(), 1);
        let buffer = terminal.backend().buffer();
        let screen = (0..20)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Pod subclass: Object"));
    }
}
