use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tui_input::Input;
use tui_input::backend::crossterm::to_input_request;

use crate::inline_terminal::{InlineTerminal, termination_flag};

const DEFAULT_HEIGHT: u16 = 20;
const MIN_HEIGHT: u16 = 8;

#[derive(Clone, Debug)]
pub struct Config {
    pub height: u16,
    pub title: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            height: DEFAULT_HEIGHT,
            title: "ininspect".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionInput {
    pub schema_version: u8,
    pub object_id: String,
    pub class_name: String,
    pub data: Value,
}

impl InspectionInput {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(anyhow!(
                "unsupported inspection schema version {}",
                self.schema_version
            ));
        }
        if self.object_id.is_empty() || self.class_name.is_empty() {
            return Err(anyhow!("inspection requires object_id and class_name"));
        }
        Ok(())
    }
}

pub fn read_input(reader: impl Read) -> Result<InspectionInput> {
    let input: InspectionInput = serde_json::from_reader(reader)?;
    input.validate()?;
    Ok(input)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct EditProposal {
    pub path: Vec<PathSegment>,
    pub old_value: Value,
    pub new_value: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Viewed,
    Proposed,
    Cancelled,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Viewed | Self::Proposed => 0,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct InspectionResult {
    pub schema_version: u8,
    pub outcome: Outcome,
    pub object_id: String,
    pub class_name: String,
    pub base_data: Option<Value>,
    pub proposal: Option<EditProposal>,
}

pub fn write_result_json(result: &InspectionResult) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, result)?;
    writeln!(stdout)?;
    Ok(())
}

pub fn run_with(input: InspectionInput, config: Config) -> Result<InspectionResult> {
    input.validate()?;
    let interrupted = termination_flag()?;
    let mut app = App::new(input, config);
    let mut terminal = InlineTerminal::enter(app.config.height.max(MIN_HEIGHT))?;

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
        if let Some(result) = handle_key(&mut app, key) {
            drop(terminal);
            return Ok(result);
        }
    }
}

#[derive(Clone, Debug)]
struct TreeRow {
    path: Vec<PathSegment>,
    label: String,
    value: Value,
    depth: usize,
    expandable: bool,
}

struct EditState {
    row: TreeRow,
    input: Input,
    error: String,
}

struct App {
    config: Config,
    inspection: InspectionInput,
    expanded: BTreeSet<Vec<PathSegment>>,
    selected: usize,
    edit: Option<EditState>,
    status: String,
}

impl App {
    fn new(inspection: InspectionInput, config: Config) -> Self {
        let mut expanded = BTreeSet::new();
        expanded.insert(Vec::new());
        Self {
            config,
            inspection,
            expanded,
            selected: 0,
            edit: None,
            status: String::new(),
        }
    }

    fn rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        append_rows(
            &self.inspection.data,
            "data".to_string(),
            Vec::new(),
            0,
            &self.expanded,
            &mut rows,
        );
        rows
    }

    fn selected_row(&self) -> Option<TreeRow> {
        self.rows().get(self.selected).cloned()
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
    }

    fn toggle(&mut self, expand: Option<bool>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.expandable {
            self.status = "e edits the selected leaf".to_string();
            return;
        }
        let is_expanded = self.expanded.contains(&row.path);
        let should_expand = expand.unwrap_or(!is_expanded);
        if should_expand {
            self.expanded.insert(row.path);
        } else {
            self.expanded.remove(&row.path);
        }
        self.clamp_selection();
    }

    fn begin_edit(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.expandable {
            self.status = "only scalar leaves can be edited".to_string();
            return;
        }
        let initial = serde_json::to_string(&row.value).unwrap_or_else(|_| "null".to_string());
        self.edit = Some(EditState {
            row,
            input: Input::from(initial),
            error: String::new(),
        });
    }

    fn result(&self, outcome: Outcome, proposal: Option<EditProposal>) -> InspectionResult {
        InspectionResult {
            schema_version: 1,
            outcome,
            object_id: self.inspection.object_id.clone(),
            class_name: self.inspection.class_name.clone(),
            base_data: proposal.as_ref().map(|_| self.inspection.data.clone()),
            proposal,
        }
    }
}

fn append_rows(
    value: &Value,
    label: String,
    path: Vec<PathSegment>,
    depth: usize,
    expanded: &BTreeSet<Vec<PathSegment>>,
    rows: &mut Vec<TreeRow>,
) {
    let expandable = matches!(value, Value::Object(_) | Value::Array(_));
    rows.push(TreeRow {
        path: path.clone(),
        label,
        value: value.clone(),
        depth,
        expandable,
    });
    if !expandable || !expanded.contains(&path) {
        return;
    }
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let mut child_path = path.clone();
                child_path.push(PathSegment::Key(key.clone()));
                append_rows(child, key.clone(), child_path, depth + 1, expanded, rows);
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                let mut child_path = path.clone();
                child_path.push(PathSegment::Index(index));
                append_rows(
                    child,
                    format!("[{index}]"),
                    child_path,
                    depth + 1,
                    expanded,
                    rows,
                );
            }
        }
        _ => {}
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<InspectionResult> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(app.result(Outcome::Cancelled, None));
    }
    if app.edit.is_some() {
        return handle_edit_key(app, key);
    }
    match key.code {
        KeyCode::Esc => return Some(app.result(Outcome::Cancelled, None)),
        KeyCode::Char('q') => return Some(app.result(Outcome::Viewed, None)),
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
            app.status.clear();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected = (app.selected + 1).min(app.rows().len().saturating_sub(1));
            app.status.clear();
        }
        KeyCode::PageUp => {
            app.selected = app.selected.saturating_sub(10);
            app.status.clear();
        }
        KeyCode::PageDown => {
            app.selected = (app.selected + 10).min(app.rows().len().saturating_sub(1));
            app.status.clear();
        }
        KeyCode::Enter | KeyCode::Char(' ') => app.toggle(None),
        KeyCode::Right | KeyCode::Char('l') => app.toggle(Some(true)),
        KeyCode::Left | KeyCode::Char('h') => app.toggle(Some(false)),
        KeyCode::Char('e') => app.begin_edit(),
        _ => {}
    }
    None
}

fn handle_edit_key(app: &mut App, key: KeyEvent) -> Option<InspectionResult> {
    match key.code {
        KeyCode::Esc => {
            app.edit = None;
            app.status = "edit cancelled".to_string();
        }
        KeyCode::Enter => {
            let edit = app.edit.as_mut().expect("edit state should exist");
            match serde_json::from_str::<Value>(edit.input.value()) {
                Ok(new_value) => {
                    let proposal = EditProposal {
                        path: edit.row.path.clone(),
                        old_value: edit.row.value.clone(),
                        new_value,
                    };
                    return Some(app.result(Outcome::Proposed, Some(proposal)));
                }
                Err(error) => edit.error = format!("invalid JSON value: {error}"),
            }
        }
        _ => {
            let event = Event::Key(key);
            if let Some(request) = to_input_request(&event) {
                let edit = app.edit.as_mut().expect("edit state should exist");
                if edit.input.handle(request).is_some() {
                    edit.error.clear();
                }
            }
        }
    }
    None
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    frame.render_widget(Clear, frame.area());
    let rows = app.rows();
    let areas = if app.edit.is_some() {
        Layout::vertical([
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area())
    } else {
        Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).split(frame.area())
    };
    draw_tree(frame, areas[0], app, &rows);

    if let Some(edit) = &app.edit {
        let editor = Paragraph::new(edit.input.value()).block(
            Block::default()
                .title(format!(" Edit {} as JSON ", display_path(&edit.row.path)))
                .borders(Borders::ALL),
        );
        frame.render_widget(editor, areas[1]);
        let cursor = edit
            .input
            .visual_cursor()
            .min(areas[1].width.saturating_sub(2) as usize);
        frame.set_cursor_position((areas[1].x + 1 + cursor as u16, areas[1].y + 1));
        let status = if edit.error.is_empty() {
            "enter propose | esc cancel edit".to_string()
        } else {
            edit.error.clone()
        };
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Yellow)),
            areas[2],
        );
    } else {
        let status = if app.status.is_empty() {
            "arrows/jk move | enter expand | e edit leaf | q close | esc cancel"
        } else {
            app.status.as_str()
        };
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
            areas[1],
        );
    }
}

fn draw_tree(frame: &mut Frame<'_>, area: Rect, app: &App, rows: &[TreeRow]) {
    let items: Vec<ListItem<'_>> = rows
        .iter()
        .map(|row| {
            let marker = if row.expandable {
                if app.expanded.contains(&row.path) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::raw("  ".repeat(row.depth)),
                Span::styled(marker, Style::default().fg(Color::Blue)),
                Span::styled(format!("{}: ", row.label), Style::default().fg(Color::Cyan)),
                Span::styled(value_summary(&row.value), value_style(&row.value)),
            ]))
        })
        .collect();
    let title = format!(
        " {} — {} {} ",
        app.config.title, app.inspection.class_name, app.inspection.object_id
    );
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn value_summary(value: &Value) -> String {
    match value {
        Value::Object(object) => format!("{{{} field(s)}}", object.len()),
        Value::Array(array) => format!("[{} item(s)]", array.len()),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}

fn value_style(value: &Value) -> Style {
    let color = match value {
        Value::Null => Color::DarkGray,
        Value::Bool(_) => Color::Magenta,
        Value::Number(_) => Color::Yellow,
        Value::String(_) => Color::Green,
        Value::Array(_) | Value::Object(_) => Color::White,
    };
    Style::default().fg(color)
}

fn display_path(path: &[PathSegment]) -> String {
    if path.is_empty() {
        return "data".to_string();
    }
    let mut display = String::new();
    for segment in path {
        match segment {
            PathSegment::Key(key) => {
                if !display.is_empty() {
                    display.push('.');
                }
                display.push_str(key);
            }
            PathSegment::Index(index) => display.push_str(&format!("[{index}]")),
        }
    }
    display
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn inspection() -> InspectionInput {
        InspectionInput {
            schema_version: 1,
            object_id: "counter_123".to_string(),
            class_name: "Counter".to_string(),
            data: serde_json::json!({"nested": {"enabled": true}, "value": 42}),
        }
    }

    #[test]
    fn parses_closed_versioned_input() {
        let json = serde_json::to_vec(&inspection()).unwrap();
        assert_eq!(read_input(Cursor::new(json)).unwrap(), inspection());

        let unknown =
            r#"{"schema_version":1,"object_id":"x","class_name":"X","data":{},"command":"oops"}"#;
        assert!(read_input(Cursor::new(unknown)).is_err());
    }

    #[test]
    fn expands_nested_objects_into_typed_paths() {
        let mut app = App::new(inspection(), Config::default());
        let rows = app.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].label, "nested");
        app.selected = 1;
        app.toggle(Some(true));
        let rows = app.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[2].path,
            vec![
                PathSegment::Key("nested".to_string()),
                PathSegment::Key("enabled".to_string())
            ]
        );
    }
}
