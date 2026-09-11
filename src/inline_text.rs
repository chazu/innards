use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Args;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ropey::Rope;
use serde::{Deserialize, Serialize};
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet};

mod render;

use crate::inline_terminal::{InlineTerminal, ResizeStep, termination_flag};

const DEFAULT_HEIGHT: u16 = 16;
const MIN_HEIGHT: u16 = 5;
const DEFAULT_FILL_COLUMN: usize = 80;
const DEFAULT_TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Edit,
    View,
}

impl Mode {
    pub fn title(self) -> &'static str {
        match self {
            Self::Edit => "inmacs",
            Self::View => "inpage",
        }
    }

    fn initial_status(self) -> &'static str {
        match self {
            Self::Edit => "Ctrl-S search  Ctrl-R reverse-search  Ctrl-X Ctrl-S save",
            Self::View => "Ctrl-S search  Ctrl-R reverse-search  Ctrl-X Ctrl-C quit",
        }
    }

    pub fn is_editable(self) -> bool {
        matches!(self, Self::Edit)
    }
}

#[derive(Args, Clone, Debug)]
pub struct CliArgs {
    /// File to edit or view.
    #[arg(
        value_name = "FILE",
        conflicts_with = "stdin",
        required_unless_present = "stdin"
    )]
    pub path: Option<PathBuf>,

    /// Read the initial buffer from stdin.
    #[arg(long, conflicts_with = "path")]
    pub stdin: bool,

    /// Save stdin-backed edits to this path.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_HEIGHT)]
    pub height: u16,

    #[arg(long)]
    pub line: Option<usize>,

    #[arg(long)]
    pub column: Option<usize>,

    #[arg(long)]
    pub title: Option<String>,

    #[arg(long)]
    pub status: Option<String>,

    #[arg(long)]
    pub tab_width: Option<usize>,

    #[arg(long)]
    pub syntax: Option<String>,

    /// Read a versioned annotation document from this path.
    #[arg(long, value_name = "FILE")]
    pub annotations: Option<PathBuf>,

    /// Write one structured outcome record to stdout after terminal cleanup.
    #[arg(long)]
    pub result_json: bool,
}

impl CliArgs {
    pub fn into_invocation(self, mode: Mode) -> Result<Invocation> {
        if self.output.is_some() && !self.stdin {
            return Err(anyhow!("--output is only valid with --stdin"));
        }
        if self.stdin && mode.is_editable() && self.output.is_none() {
            return Err(anyhow!("inmacs --stdin requires --output FILE"));
        }
        if self.tab_width == Some(0) {
            return Err(anyhow!("--tab-width must be greater than zero"));
        }

        let annotations = match self.annotations {
            Some(path) => AnnotationDocument::read(&path)?.annotations,
            None => Vec::new(),
        };
        let tab_width = self.tab_width.unwrap_or_else(|| {
            if is_trashtalk_syntax(self.syntax.as_deref(), self.path.as_deref()) {
                2
            } else {
                DEFAULT_TAB_WIDTH
            }
        });

        Ok(Invocation {
            config: Config {
                mode,
                input_path: self.path,
                output_path: self.output,
                height: self.height.max(MIN_HEIGHT),
                line: self.line.map(|line| line.max(1)),
                column: self.column.map(|column| column.max(1)),
                title: self.title,
                status: self.status,
                tab_width,
                syntax: self.syntax,
                annotations,
            },
            result_json: self.result_json,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Invocation {
    pub config: Config,
    pub result_json: bool,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub input_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
    pub height: u16,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub tab_width: usize,
    pub syntax: Option<String>,
    pub annotations: Vec<Annotation>,
}

impl Config {
    pub fn for_path(mode: Mode, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let tab_width = if is_trashtalk_syntax(None, Some(&path)) {
            2
        } else {
            DEFAULT_TAB_WIDTH
        };
        Self {
            mode,
            input_path: Some(path),
            output_path: None,
            height: DEFAULT_HEIGHT,
            line: None,
            column: None,
            title: None,
            status: None,
            tab_width,
            syntax: None,
            annotations: Vec::new(),
        }
    }
}

pub fn run_with(config: Config) -> Result<RunResult> {
    let mut app = Editor::open(&config)?;
    let syntax = SyntaxHighlighter::new(config.syntax.as_deref(), config.input_path.as_deref())?;
    let interrupted = termination_flag()?;

    let mut terminal = InlineTerminal::enter(config.height)?;
    if config.mode == Mode::View {
        terminal.enable_mouse_capture()?;
    }
    terminal.draw(|frame| render::draw(frame, &mut app, &syntax, config.mode))?;
    let outcome = run_editor(&mut terminal, &mut app, &syntax, config.mode, &interrupted)?;
    drop(terminal);

    Ok(app.result(outcome))
}

pub fn write_result_json(result: &RunResult) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, result)?;
    writeln!(stdout)?;
    Ok(())
}

pub fn normalize_plus_line_args<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut normalized = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        if index > 0
            && let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix('+'))
            && !value.is_empty()
            && value.chars().all(|ch| ch.is_ascii_digit())
        {
            normalized.push("--line".into());
            normalized.push(value.into());
        } else if arg == "-" {
            normalized.push("--stdin".into());
        } else {
            normalized.push(arg);
        }
    }
    normalized
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Annotation {
    pub line: usize,
    #[serde(default = "one")]
    pub column: usize,
    pub severity: AnnotationSeverity,
    pub message: String,
}

fn one() -> usize {
    1
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AnnotationDocument {
    pub schema_version: u8,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

impl AnnotationDocument {
    pub fn read(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read annotations from {}", path.display()))?;
        let document: Self = serde_json::from_str(&contents)
            .with_context(|| format!("invalid annotations in {}", path.display()))?;
        if document.schema_version != 1 {
            return Err(anyhow!(
                "unsupported annotation schema version {}",
                document.schema_version
            ));
        }
        if document
            .annotations
            .iter()
            .any(|item| item.line == 0 || item.column == 0)
        {
            return Err(anyhow!("annotation positions must be one-based"));
        }
        Ok(document)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Saved,
    Unchanged,
    Discarded,
    Closed,
    Cancelled,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Saved | Self::Unchanged | Self::Closed => 0,
            Self::Discarded => 3,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CursorResult {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunResult {
    pub schema_version: u8,
    pub outcome: Outcome,
    pub path: Option<String>,
    pub changed: bool,
    pub cursor: CursorResult,
    pub edit_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchDirection {
    Forward,
    Reverse,
}

struct SearchState {
    query: String,
    direction: SearchDirection,
    origin_line: usize,
    origin_col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BufferPoint {
    line: usize,
    col: usize,
}

#[derive(Clone)]
struct EditorSnapshot {
    buffer: Rope,
    cursor_line: usize,
    cursor_col: usize,
    dirty: bool,
    mark: Option<BufferPoint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DiskState {
    Missing,
    Present(String),
}

impl DiskState {
    fn read(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(Self::Present(contents)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }
}

pub struct Editor {
    path: Option<PathBuf>,
    display_path: String,
    initial_content: String,
    expected_disk_state: DiskState,
    conflict_confirmation: Option<DiskState>,
    buffer: Rope,
    cursor_line: usize,
    cursor_col: usize,
    scroll_y: usize,
    scroll_x: usize,
    kill_ring: String,
    dirty: bool,
    status: String,
    ctrl_x_pending: bool,
    height: u16,
    last_drawn_height: u16,
    last_drawn_top: u16,
    search: Option<SearchState>,
    mark: Option<BufferPoint>,
    undo_stack: Vec<EditorSnapshot>,
    redo_stack: Vec<EditorSnapshot>,
    edit_count: usize,
    save_count: usize,
    tab_width: usize,
    annotations: Vec<Annotation>,
}

impl Editor {
    pub fn open(config: &Config) -> Result<Self> {
        let (content, default_display_path) = match config.input_path.as_ref() {
            Some(path) => {
                let content = match fs::read_to_string(path) {
                    Ok(content) => content,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
                    Err(err) => {
                        return Err(err)
                            .with_context(|| format!("failed to read {}", path.display()));
                    }
                };
                (content, path.display().to_string())
            }
            None => {
                let mut content = String::new();
                io::stdin()
                    .read_to_string(&mut content)
                    .context("failed to read stdin")?;
                (content, "<stdin>".to_string())
            }
        };
        let path = config
            .output_path
            .clone()
            .or_else(|| config.input_path.clone());
        if config.mode.is_editable()
            && let Some(path) = path.as_deref()
        {
            reject_symlink(path)?;
        }
        let expected_disk_state = match path.as_deref() {
            Some(path) => DiskState::read(path)?,
            None => DiskState::Missing,
        };
        let display_path = config.title.clone().unwrap_or_else(|| {
            config
                .output_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or(default_display_path)
        });
        let buffer = Rope::from_str(&content);
        let cursor_line = config
            .line
            .map(|line| {
                line.saturating_sub(1)
                    .min(buffer_line_count(&buffer).saturating_sub(1))
            })
            .unwrap_or(0);
        let cursor_col = config
            .column
            .map(|column| column.saturating_sub(1))
            .unwrap_or(0)
            .min(line_len_chars(&buffer, cursor_line));

        Ok(Self {
            path,
            display_path,
            initial_content: content,
            expected_disk_state,
            conflict_confirmation: None,
            buffer,
            cursor_line,
            cursor_col,
            scroll_y: 0,
            scroll_x: 0,
            kill_ring: String::new(),
            dirty: false,
            status: config
                .status
                .clone()
                .unwrap_or_else(|| config.mode.initial_status().to_string()),
            ctrl_x_pending: false,
            height: config.height.max(MIN_HEIGHT),
            last_drawn_height: config.height.max(MIN_HEIGHT),
            last_drawn_top: 0,
            search: None,
            mark: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            edit_count: 0,
            save_count: 0,
            tab_width: config.tab_width,
            annotations: config.annotations.clone(),
        })
    }

    fn save(&mut self) -> Result<bool> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("stdin buffer has no output path"))?;
        reject_symlink(path)?;
        let current_disk_state = DiskState::read(path)?;
        if current_disk_state != self.expected_disk_state
            && self.conflict_confirmation.as_ref() != Some(&current_disk_state)
        {
            self.conflict_confirmation = Some(current_disk_state);
            self.status = "file changed on disk; Ctrl-X Ctrl-S again to overwrite".to_string();
            return Ok(false);
        }

        let contents = self.buffer.to_string();
        atomic_write(path, contents.as_bytes())?;
        self.expected_disk_state = DiskState::Present(contents);
        self.conflict_confirmation = None;
        self.dirty = false;
        self.save_count += 1;
        self.status = format!("saved {}", path.display());
        Ok(true)
    }

    fn result(&self, outcome: Outcome) -> RunResult {
        RunResult {
            schema_version: 1,
            outcome,
            path: self.path.as_ref().map(|path| path.display().to_string()),
            changed: self.buffer != self.initial_content,
            cursor: CursorResult {
                line: self.cursor_line + 1,
                column: self.cursor_col + 1,
            },
            edit_count: self.edit_count,
        }
    }

    fn quit_outcome(&self, mode: Mode) -> Outcome {
        if mode == Mode::View {
            Outcome::Closed
        } else if self.dirty {
            Outcome::Discarded
        } else if self.save_count > 0 {
            Outcome::Saved
        } else {
            Outcome::Unchanged
        }
    }

    fn line_len(&self) -> usize {
        line_len_chars(&self.buffer, self.cursor_line)
    }

    fn line_count(&self) -> usize {
        buffer_line_count(&self.buffer)
    }

    fn cursor_char_idx(&self) -> usize {
        line_start_char(&self.buffer, self.cursor_line) + self.cursor_col
    }

    fn cursor_point(&self) -> BufferPoint {
        BufferPoint {
            line: self.cursor_line,
            col: self.cursor_col,
        }
    }

    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            buffer: self.buffer.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            dirty: self.dirty,
            mark: self.mark,
        }
    }

    fn restore_snapshot(&mut self, snapshot: EditorSnapshot) {
        self.buffer = snapshot.buffer;
        self.cursor_line = snapshot.cursor_line;
        self.cursor_col = snapshot.cursor_col;
        self.dirty = snapshot.dirty;
        self.mark = snapshot.mark;
        self.search = None;
        self.ctrl_x_pending = false;
        self.clamp_cursor();
    }

    fn record_edit(&mut self) {
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();
        self.edit_count += 1;
    }

    fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            self.status = "no undo".to_string();
            return;
        };
        self.redo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status = "undo".to_string();
    }

    fn redo(&mut self) {
        let Some(snapshot) = self.redo_stack.pop() else {
            self.status = "no redo".to_string();
            return;
        };
        self.undo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status = "redo".to_string();
    }

    fn point_char_idx(&self, point: BufferPoint) -> usize {
        let line = point.line.min(self.line_count().saturating_sub(1));
        line_start_char(&self.buffer, line) + point.col.min(line_len_chars(&self.buffer, line))
    }

    fn set_cursor_from_char_idx(&mut self, char_idx: usize) {
        let char_idx = char_idx.min(self.buffer.len_chars());
        self.cursor_line = self.buffer.char_to_line(char_idx);
        self.cursor_col = char_idx.saturating_sub(line_start_char(&self.buffer, self.cursor_line));
        self.clamp_cursor();
    }

    fn active_region(&self) -> Option<Range<usize>> {
        let mark = self.mark?;
        let mark = self.point_char_idx(mark);
        let cursor = self.cursor_char_idx();
        if mark == cursor {
            return None;
        }
        Some(mark.min(cursor)..mark.max(cursor))
    }

    fn delete_active_region(&mut self) -> bool {
        let Some(region) = self.active_region() else {
            return false;
        };
        self.remove_region(region);
        self.mark_dirty();
        true
    }

    fn remove_region(&mut self, region: Range<usize>) -> String {
        let start = region.start;
        let text = self.buffer.slice(region.clone()).to_string();
        self.buffer.remove(region);
        self.set_cursor_from_char_idx(start);
        text
    }

    fn clamp_cursor(&mut self) {
        self.cursor_line = self.cursor_line.min(self.line_count().saturating_sub(1));
        self.cursor_col = self.cursor_col.min(self.line_len());
    }

    fn ensure_cursor_visible(&mut self, text_height: usize, text_width: usize) {
        if self.cursor_line < self.scroll_y {
            self.scroll_y = self.cursor_line;
        } else if self.cursor_line >= self.scroll_y.saturating_add(text_height) {
            self.scroll_y = self
                .cursor_line
                .saturating_sub(text_height.saturating_sub(1));
        }

        if self.cursor_col < self.scroll_x {
            self.scroll_x = self.cursor_col;
        } else if self.cursor_col >= self.scroll_x.saturating_add(text_width) {
            self.scroll_x = self.cursor_col.saturating_sub(text_width.saturating_sub(1));
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if mouse.row < self.last_drawn_top
            || mouse.row >= self.last_drawn_top.saturating_add(self.last_drawn_height)
        {
            return;
        }
        let text_height = self.last_drawn_height.saturating_sub(3).max(1) as usize;
        let max_scroll = self.line_count().saturating_sub(text_height);
        self.scroll_y = match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_y.saturating_sub(3),
            MouseEventKind::ScrollDown => self.scroll_y.saturating_add(3).min(max_scroll),
            _ => return,
        };
        // Keep point visible so rendering and subsequent keyboard navigation
        // continue from the scrolled viewport instead of snapping back.
        self.cursor_line = self
            .cursor_line
            .clamp(self.scroll_y, self.scroll_y + text_height - 1);
        self.clamp_cursor();
    }

    fn insert_char(&mut self, ch: char) {
        self.record_edit();
        self.delete_active_region();
        self.buffer.insert_char(self.cursor_char_idx(), ch);
        self.cursor_col += 1;
        self.mark_dirty();
    }

    fn insert_newline(&mut self) {
        let indent: String = line_text(&self.buffer, self.cursor_line)
            .chars()
            .take(self.cursor_col)
            .take_while(|character| character.is_whitespace())
            .collect();
        self.record_edit();
        self.delete_active_region();
        let insertion = format!("\n{indent}");
        self.buffer.insert(self.cursor_char_idx(), &insertion);
        self.cursor_line += 1;
        self.cursor_col = indent.chars().count();
        self.mark_dirty();
    }

    fn insert_tab(&mut self) {
        let width = self.tab_width - (self.cursor_col % self.tab_width);
        self.record_edit();
        self.delete_active_region();
        self.buffer
            .insert(self.cursor_char_idx(), &" ".repeat(width));
        self.cursor_col += width;
        self.mark_dirty();
    }

    fn backspace(&mut self) {
        if self.active_region().is_some() {
            self.record_edit();
            self.delete_active_region();
            return;
        }
        if self.cursor_col > 0 {
            self.record_edit();
            let end = self.cursor_char_idx();
            self.buffer.remove(end - 1..end);
            self.cursor_col -= 1;
            self.mark_dirty();
        } else if self.cursor_line > 0 {
            self.record_edit();
            let current_start = line_start_char(&self.buffer, self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.line_len();
            let previous_content_end =
                line_start_char(&self.buffer, self.cursor_line) + self.cursor_col;
            self.buffer.remove(previous_content_end..current_start);
            self.mark_dirty();
        }
    }

    fn delete_char(&mut self) {
        if self.active_region().is_some() {
            self.record_edit();
            self.delete_active_region();
            return;
        }
        if self.cursor_col < self.line_len() {
            self.record_edit();
            let start = self.cursor_char_idx();
            self.buffer.remove(start..start + 1);
            self.mark_dirty();
        } else if self.cursor_line + 1 < self.line_count() {
            self.record_edit();
            let start = self.cursor_char_idx();
            let end = line_start_char(&self.buffer, self.cursor_line + 1);
            self.buffer.remove(start..end);
            self.mark_dirty();
        }
    }

    fn kill_to_eol(&mut self) {
        if self.cursor_col < self.line_len() {
            self.record_edit();
            let start = self.cursor_char_idx();
            let end = line_start_char(&self.buffer, self.cursor_line) + self.line_len();
            self.kill_ring = self.buffer.slice(start..end).to_string();
            self.buffer.remove(start..end);
        } else if self.cursor_line + 1 < self.line_count() {
            self.record_edit();
            let start = self.cursor_char_idx();
            let end = line_start_char(&self.buffer, self.cursor_line + 1);
            self.kill_ring = self.buffer.slice(start..end).to_string();
            self.buffer.remove(start..end);
        } else {
            self.kill_ring.clear();
            return;
        }
        self.mark_dirty();
    }

    fn toggle_mark(&mut self) {
        let point = self.cursor_point();
        if self.mark == Some(point) {
            self.mark = None;
            self.status = "mark cleared".to_string();
        } else {
            self.mark = Some(point);
            self.status = "mark set".to_string();
        }
        self.ctrl_x_pending = false;
    }

    fn cancel_mark(&mut self) {
        if self.mark.take().is_some() {
            self.status = "mark cancelled".to_string();
        } else {
            self.status = "quit".to_string();
        }
        self.ctrl_x_pending = false;
    }

    fn copy_region(&mut self) {
        let Some(region) = self.active_region() else {
            self.status = "no active region".to_string();
            return;
        };
        self.kill_ring = self.buffer.slice(region).to_string();
        self.status = "region copied".to_string();
        self.ctrl_x_pending = false;
    }

    fn kill_region(&mut self) {
        let Some(region) = self.active_region() else {
            self.status = "no active region".to_string();
            return;
        };
        self.record_edit();
        self.kill_ring = self.remove_region(region);
        self.mark_dirty();
        self.status = "region killed".to_string();
    }

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        self.record_edit();
        self.delete_active_region();
        let text = self.kill_ring.clone();
        for ch in text.chars() {
            if ch == '\n' {
                self.buffer.insert_char(self.cursor_char_idx(), '\n');
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else {
                self.buffer.insert_char(self.cursor_char_idx(), ch);
                self.cursor_col += 1;
            }
        }
        self.mark_dirty();
    }

    fn fill_paragraph(&mut self, column: usize) {
        let Some((start_line, end_line)) = self.paragraph_bounds(self.cursor_line) else {
            self.status = "no paragraph".to_string();
            return;
        };

        let original_cursor = self.cursor_char_idx();
        let start = line_start_char(&self.buffer, start_line);
        let last_line = end_line - 1;
        let end =
            line_start_char(&self.buffer, last_line) + line_len_chars(&self.buffer, last_line);
        let original = self.buffer.slice(start..end).to_string();
        let lines: Vec<String> = (start_line..end_line)
            .map(|line| line_text(&self.buffer, line))
            .collect();

        let indent_len = common_indent_len(&lines);
        let indent: String = lines
            .iter()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.chars().take(indent_len).collect())
            .unwrap_or_default();
        let mut words = Vec::new();
        for line in &lines {
            let content: String = line.chars().skip(indent_len).collect();
            words.extend(content.split_whitespace().map(str::to_string));
        }

        if words.is_empty() {
            self.status = "no paragraph".to_string();
            return;
        }

        let wrapped = wrap_words(&words, &indent, column);
        if wrapped == original {
            self.status = format!("already filled to {column}");
            return;
        }

        self.record_edit();
        self.buffer.remove(start..end);
        self.buffer.insert(start, &wrapped);
        let cursor = start
            + original_cursor
                .saturating_sub(start)
                .min(char_len(&wrapped));
        self.set_cursor_from_char_idx(cursor);
        self.mark_dirty();
        self.status = format!("filled paragraph to {column}");
    }

    fn paragraph_bounds(&self, line: usize) -> Option<(usize, usize)> {
        if self.line_is_blank(line) {
            return None;
        }

        let mut start = line;
        while start > 0 && !self.line_is_blank(start - 1) {
            start -= 1;
        }

        let mut end = line + 1;
        while end < self.line_count() && !self.line_is_blank(end) {
            end += 1;
        }

        Some((start, end))
    }

    fn line_is_blank(&self, line: usize) -> bool {
        line_text(&self.buffer, line).trim().is_empty()
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.line_len();
        }
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.line_len() {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.line_count() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.clamp_cursor();
        }
    }

    fn move_down(&mut self) {
        if self.cursor_line + 1 < self.line_count() {
            self.cursor_line += 1;
            self.clamp_cursor();
        }
    }

    fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            self.move_left();
            return;
        }
        let chars: Vec<char> = line_text(&self.buffer, self.cursor_line).chars().collect();
        let mut col = self.cursor_col;
        while col > 0 && chars[col - 1].is_whitespace() {
            col -= 1;
        }
        while col > 0 && !chars[col - 1].is_whitespace() {
            col -= 1;
        }
        self.cursor_col = col;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = line_text(&self.buffer, self.cursor_line).chars().collect();
        let mut col = self.cursor_col;
        while col < chars.len() && !chars[col].is_whitespace() {
            col += 1;
        }
        while col < chars.len() && chars[col].is_whitespace() {
            col += 1;
        }
        self.cursor_col = col;
    }

    fn page_up(&mut self) {
        let amount = self.page_amount();
        self.cursor_line = self.cursor_line.saturating_sub(amount);
        self.clamp_cursor();
    }

    fn page_down(&mut self) {
        let amount = self.page_amount();
        self.cursor_line = (self.cursor_line + amount).min(self.line_count().saturating_sub(1));
        self.clamp_cursor();
    }

    fn page_amount(&self) -> usize {
        usize::from(self.height.saturating_sub(3)).max(1)
    }

    fn begin_search(&mut self, direction: SearchDirection) {
        self.ctrl_x_pending = false;
        self.search = Some(SearchState {
            query: String::new(),
            direction,
            origin_line: self.cursor_line,
            origin_col: self.cursor_col,
        });
        self.update_search_status();
    }

    fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.cursor_line = search.origin_line;
            self.cursor_col = search.origin_col;
            self.status = "search cancelled".to_string();
        }
    }

    fn finish_search(&mut self) {
        if self.search.take().is_some() {
            self.status = "search done".to_string();
        }
    }

    fn search_insert_char(&mut self, ch: char) {
        if let Some(search) = self.search.as_mut() {
            search.query.push(ch);
        }
        self.apply_incremental_search();
    }

    fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.query.pop();
        }
        self.apply_incremental_search();
    }

    fn search_repeat(&mut self, direction: SearchDirection) {
        if let Some(search) = self.search.as_mut() {
            search.direction = direction;
        }
        self.apply_repeated_search();
    }

    fn apply_incremental_search(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        if search.query.is_empty() {
            self.cursor_line = search.origin_line;
            self.cursor_col = search.origin_col;
            self.update_search_status();
            return;
        }

        let found = self.find_from(
            search.direction,
            &search.query,
            search.origin_line,
            search.origin_col,
        );

        if let Some((line, col)) = found {
            self.cursor_line = line;
            self.cursor_col = col;
        }
        self.update_search_status();
    }

    fn apply_repeated_search(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        if search.query.is_empty() {
            self.update_search_status();
            return;
        }

        let (line, col) = match search.direction {
            SearchDirection::Forward => (self.cursor_line, self.cursor_col.saturating_add(1)),
            SearchDirection::Reverse => (self.cursor_line, self.cursor_col),
        };
        let found = self.find_from(search.direction, &search.query, line, col);

        if let Some((line, col)) = found {
            self.cursor_line = line;
            self.cursor_col = col;
        }
        self.update_search_status();
    }

    fn find_from(
        &self,
        direction: SearchDirection,
        query: &str,
        start_line: usize,
        start_col: usize,
    ) -> Option<(usize, usize)> {
        match direction {
            SearchDirection::Forward => self.find_forward(query, start_line, start_col),
            SearchDirection::Reverse => self.find_reverse(query, start_line, start_col),
        }
    }

    fn update_search_status(&mut self) {
        if let Some(search) = self.search.as_ref() {
            let label = match search.direction {
                SearchDirection::Forward => "I-search",
                SearchDirection::Reverse => "I-search backward",
            };
            self.status = format!("{label}: {}", search.query);
        }
    }

    fn find_forward(
        &self,
        query: &str,
        start_line: usize,
        start_col: usize,
    ) -> Option<(usize, usize)> {
        let line_count = self.line_count();
        for line_idx in start_line..line_count {
            let start = if line_idx == start_line { start_col } else { 0 };
            if let Some(col) =
                find_in_line_forward(&line_text(&self.buffer, line_idx), query, start)
            {
                return Some((line_idx, col));
            }
        }
        for line_idx in 0..start_line {
            if let Some(col) = find_in_line_forward(&line_text(&self.buffer, line_idx), query, 0) {
                return Some((line_idx, col));
            }
        }
        None
    }

    fn find_reverse(
        &self,
        query: &str,
        start_line: usize,
        start_col: usize,
    ) -> Option<(usize, usize)> {
        for line_idx in (0..=start_line).rev() {
            let end = if line_idx == start_line {
                start_col
            } else {
                line_len_chars(&self.buffer, line_idx)
            };
            if let Some(col) = find_in_line_reverse(&line_text(&self.buffer, line_idx), query, end)
            {
                return Some((line_idx, col));
            }
        }
        for line_idx in ((start_line + 1)..self.line_count()).rev() {
            let line = line_text(&self.buffer, line_idx);
            if let Some(col) = find_in_line_reverse(&line, query, char_len(&line)) {
                return Some((line_idx, col));
            }
        }
        None
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.ctrl_x_pending = false;
        self.search = None;
        self.mark = None;
        self.status = "modified".to_string();
    }

    fn annotation_for_line(&self, line: usize) -> Option<&Annotation> {
        self.annotations
            .iter()
            .find(|annotation| annotation.line == line + 1)
    }
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "refusing to edit symlink {}; edit its target explicitly",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect output path {}", path.display()))
        }
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("output path has no file name: {}", path.display()))?
        .to_string_lossy();
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());

    let mut temporary = None;
    for attempt in 0..100_u8 {
        let candidate = parent.join(format!(
            ".{name}.innards.{}.{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create temporary file beside {}", path.display())
                });
            }
        }
    }

    let (temporary_path, mut file) = temporary.ok_or_else(|| {
        anyhow!(
            "could not allocate a temporary file beside {}",
            path.display()
        )
    })?;
    let result = (|| -> Result<()> {
        file.write_all(contents)
            .with_context(|| format!("failed to write {}", temporary_path.display()))?;
        file.flush()?;
        file.sync_all()?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary_path, permissions)?;
        }
        drop(file);
        fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                temporary_path.display()
            )
        })?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
    syntax_name: String,
}

impl SyntaxHighlighter {
    fn new(requested: Option<&str>, path: Option<&Path>) -> Result<Self> {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        let trashtalk = SyntaxDefinition::load_from_str(
            include_str!("../syntaxes/Trashtalk.sublime-syntax"),
            true,
            None,
        )
        .context("invalid bundled Trashtalk syntax definition")?;
        builder.add(trashtalk);
        let syntax_set = builder.build();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .or_else(|| theme_set.themes.values().next())
            .cloned()
            .ok_or_else(|| anyhow!("no syntect themes available"))?;
        let syntax = if is_trashtalk_syntax(requested, path) {
            syntax_set
                .find_syntax_by_name("Trashtalk")
                .expect("bundled Trashtalk syntax should be present")
        } else if let Some(requested) = requested {
            syntax_set
                .find_syntax_by_token(requested)
                .or_else(|| syntax_set.find_syntax_by_name(requested))
                .ok_or_else(|| anyhow!("unknown syntax: {requested}"))?
        } else {
            path.and_then(|path| syntax_set.find_syntax_for_file(path).ok().flatten())
                .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
        };
        let syntax_name = syntax.name.clone();

        Ok(Self {
            syntax_set,
            theme,
            syntax_name,
        })
    }

    fn syntax<'a>(&'a self) -> &'a SyntaxReference {
        self.syntax_set
            .find_syntax_by_name(&self.syntax_name)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }
}

fn is_trashtalk_syntax(requested: Option<&str>, path: Option<&Path>) -> bool {
    requested.is_some_and(|name| {
        name.eq_ignore_ascii_case("trashtalk") || name.eq_ignore_ascii_case("trash")
    }) || requested.is_none()
        && path.is_some_and(|path| {
            path.extension()
                .is_some_and(|extension| extension == "trash")
        })
}

fn run_editor(
    terminal: &mut InlineTerminal,
    app: &mut Editor,
    syntax: &SyntaxHighlighter,
    mode: Mode,
    interrupted: &AtomicBool,
) -> Result<Outcome> {
    loop {
        if interrupted.load(Ordering::Relaxed) {
            return Ok(Outcome::Cancelled);
        }

        let ready = match event::poll(Duration::from_millis(80)) {
            Ok(ready) => ready,
            Err(_err) if interrupted.load(Ordering::Relaxed) => {
                return Ok(Outcome::Cancelled);
            }
            Err(err) => return Err(err.into()),
        };
        if ready {
            match event::read() {
                Ok(Event::Key(key))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let Some(outcome) = handle_key(app, key, terminal, mode)? {
                        return Ok(outcome);
                    }
                }
                Ok(Event::Mouse(mouse)) if mode == Mode::View => app.handle_mouse(mouse),
                Ok(_) => {}
                Err(err) => {
                    app.status = format!("input error: {err}");
                }
            }
        }
        terminal.draw(|frame| render::draw(frame, app, syntax, mode))?;
    }
}

fn handle_key(
    app: &mut Editor,
    key: KeyEvent,
    terminal: &mut InlineTerminal,
    mode: Mode,
) -> Result<Option<Outcome>> {
    // Resize before search/edit handling so the chord changes only geometry.
    // Keep the editor's existing C-x save/quit state and dispatch intact.
    if let Some(step) = ResizeStep::from_key(key, app.ctrl_x_pending) {
        app.ctrl_x_pending = false;
        terminal.resize_by(step, MIN_HEIGHT)?;
        app.height = terminal.height();
        app.status = format!("height {}", app.height);
        return Ok(None);
    }
    if app.ctrl_x_pending {
        return handle_ctrl_x_chord(app, key, mode);
    }

    if app.search.is_some() {
        return handle_search_key(app, key);
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(Some(Outcome::Cancelled));
        }
        _ if is_view_quit_key(mode, key) => return Ok(Some(app.quit_outcome(mode))),
        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.ctrl_x_pending = true;
            app.status = "Ctrl-X ...".to_string();
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.begin_search(SearchDirection::Forward);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.begin_search(SearchDirection::Reverse);
        }
        KeyCode::Char('/')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.undo()
        }
        KeyCode::Char('_')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.undo()
        }
        KeyCode::Char('7')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.undo()
        }
        KeyCode::Char('?')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.redo()
        }
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_mark(),
        KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => app.toggle_mark(),
        KeyCode::Null
            if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.toggle_mark();
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_col = 0,
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cursor_col = app.line_len();
        }
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => app.move_word_left(),
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => app.move_word_right(),
        KeyCode::Char('q') if mode.is_editable() && key.modifiers.contains(KeyModifiers::ALT) => {
            app.fill_paragraph(DEFAULT_FILL_COLUMN)
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::ALT) => app.copy_region(),
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => app.move_left(),
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => app.move_right(),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => app.move_up(),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => app.move_down(),
        KeyCode::Char('d')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.delete_char()
        }
        KeyCode::Char('k')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.kill_to_eol()
        }
        KeyCode::Char('w')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.kill_region()
        }
        KeyCode::Char('y')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.yank()
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::ALT) => app.page_up(),
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => app.page_down(),
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => app.move_word_left(),
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => app.move_word_right(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up => app.move_up(),
        KeyCode::Down => app.move_down(),
        KeyCode::Home => app.cursor_col = 0,
        KeyCode::End => app.cursor_col = app.line_len(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::Backspace if mode.is_editable() => app.backspace(),
        KeyCode::Delete if mode.is_editable() => app.delete_char(),
        KeyCode::Enter if mode.is_editable() => app.insert_newline(),
        KeyCode::Tab if mode.is_editable() => app.insert_tab(),
        KeyCode::Char(ch)
            if mode.is_editable()
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            app.insert_char(ch);
        }
        _ => {}
    }

    Ok(None)
}

fn is_view_quit_key(mode: Mode, key: KeyEvent) -> bool {
    mode == Mode::View
        && matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') if key.modifiers.is_empty()
        )
}

fn handle_ctrl_x_chord(app: &mut Editor, key: KeyEvent, mode: Mode) -> Result<Option<Outcome>> {
    app.ctrl_x_pending = false;
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Ok(Some(app.quit_outcome(mode)))
        }
        KeyCode::Char('s')
            if mode.is_editable() && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.save()?;
            Ok(None)
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.status = "read-only".to_string();
            Ok(None)
        }
        _ => {
            app.status = "unknown Ctrl-X sequence".to_string();
            Ok(None)
        }
    }
}

fn handle_search_key(app: &mut Editor, key: KeyEvent) -> Result<Option<Outcome>> {
    match key.code {
        KeyCode::Esc => app.cancel_search(),
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_search(),
        KeyCode::Enter => app.finish_search(),
        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.ctrl_x_pending = true;
            app.status = "Ctrl-X ...".to_string();
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_repeat(SearchDirection::Forward);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_repeat(SearchDirection::Reverse);
        }
        KeyCode::Backspace => app.search_backspace(),
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            app.search_insert_char(ch);
        }
        _ => {}
    }

    Ok(None)
}

fn line_selection_range(
    app: &Editor,
    line: usize,
    active_region: Option<&Range<usize>>,
) -> Option<Range<usize>> {
    let active_region = active_region?;
    let line_start = line_start_char(&app.buffer, line);
    let line_end = line_start + line_len_chars(&app.buffer, line);
    let start = active_region.start.max(line_start);
    let end = active_region.end.min(line_end);
    if start < end {
        Some(start - line_start..end - line_start)
    } else {
        None
    }
}

fn buffer_line_count(buffer: &Rope) -> usize {
    buffer.len_lines().max(1)
}

fn line_start_char(buffer: &Rope, line: usize) -> usize {
    buffer.line_to_char(line.min(buffer_line_count(buffer).saturating_sub(1)))
}

fn line_len_chars(buffer: &Rope, line: usize) -> usize {
    let line = buffer.line(line.min(buffer_line_count(buffer).saturating_sub(1)));
    let mut len = line.len_chars();
    if len > 0 && line.char(len - 1) == '\n' {
        len -= 1;
        if len > 0 && line.char(len - 1) == '\r' {
            len -= 1;
        }
    }
    len
}

fn line_text(buffer: &Rope, line: usize) -> String {
    let line = line.min(buffer_line_count(buffer).saturating_sub(1));
    let len = line_len_chars(buffer, line);
    buffer.line(line).slice(..len).to_string()
}

fn common_indent_len(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|ch| ch.is_whitespace()).count())
        .min()
        .unwrap_or(0)
}

fn wrap_words(words: &[String], indent: &str, column: usize) -> String {
    let indent_len = char_len(indent);
    let target = column.max(indent_len + 1);
    let mut lines = Vec::new();
    let mut current = indent.to_string();
    let mut current_len = indent_len;

    for word in words {
        let word_len = char_len(word);
        let needs_space = current_len > indent_len;
        let next_len = current_len + usize::from(needs_space) + word_len;

        if needs_space && next_len > target {
            lines.push(current);
            current = indent.to_string();
            current.push_str(word);
            current_len = indent_len + word_len;
        } else {
            if needs_space {
                current.push(' ');
                current_len += 1;
            }
            current.push_str(word);
            current_len += word_len;
        }
    }

    lines.push(current);
    lines.join("\n")
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn find_in_line_forward(line: &str, query: &str, start_col: usize) -> Option<usize> {
    let start = byte_index(line, start_col);
    line.get(start..)?
        .find(query)
        .map(|idx| start_col + char_len(&line[start..start + idx]))
}

fn find_in_line_reverse(line: &str, query: &str, end_col: usize) -> Option<usize> {
    let end = byte_index(line, end_col);
    line.get(..end)?
        .rfind(query)
        .map(|idx| char_len(&line[..idx]))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(test_name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "innards-unit-{test_name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn editor_with(text: &str) -> Editor {
        Editor {
            path: Some(PathBuf::from("test.txt")),
            display_path: "test.txt".to_string(),
            initial_content: text.to_string(),
            expected_disk_state: DiskState::Present(text.to_string()),
            conflict_confirmation: None,
            buffer: Rope::from_str(text),
            cursor_line: 0,
            cursor_col: 0,
            scroll_y: 0,
            scroll_x: 0,
            kill_ring: String::new(),
            dirty: false,
            status: String::new(),
            ctrl_x_pending: false,
            height: DEFAULT_HEIGHT,
            last_drawn_height: DEFAULT_HEIGHT,
            last_drawn_top: 0,
            search: None,
            mark: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            edit_count: 0,
            save_count: 0,
            tab_width: DEFAULT_TAB_WIDTH,
            annotations: Vec::new(),
        }
    }

    fn cli_args() -> CliArgs {
        CliArgs {
            path: None,
            stdin: true,
            output: None,
            height: DEFAULT_HEIGHT,
            line: None,
            column: None,
            title: None,
            status: None,
            tab_width: None,
            syntax: None,
            annotations: None,
            result_json: true,
        }
    }

    #[test]
    fn mouse_wheel_scrolls_viewport_and_clamps_at_buffer_edges() {
        let mut editor = editor_with(
            &(0..40)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let wheel = |kind| MouseEvent {
            kind,
            column: 5,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };
        editor.handle_mouse(wheel(MouseEventKind::ScrollDown));
        editor.ensure_cursor_visible(13, 60);
        assert_eq!(editor.scroll_y, 3);
        assert_eq!(editor.cursor_line, 3);
        for _ in 0..100 {
            editor.handle_mouse(wheel(MouseEventKind::ScrollDown));
        }
        assert_eq!(editor.scroll_y, 27);
        for _ in 0..100 {
            editor.handle_mouse(wheel(MouseEventKind::ScrollUp));
        }
        editor.ensure_cursor_visible(13, 60);
        assert_eq!(editor.scroll_y, 0);
        assert!(!editor.dirty);
        assert_eq!(editor.edit_count, 0);
    }

    #[test]
    fn mouse_wheel_ignores_rows_outside_inline_pager_and_short_buffers() {
        let mut editor = editor_with("one\ntwo");
        editor.last_drawn_top = 5;
        for row in [0, 5, 20, 21] {
            editor.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row,
                modifiers: KeyModifiers::empty(),
            });
            assert_eq!(editor.scroll_y, 0);
        }
        editor.buffer = Rope::from_str(&"line\n".repeat(40));
        for row in [4, 21] {
            editor.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row,
                modifiers: KeyModifiers::empty(),
            });
            assert_eq!(editor.scroll_y, 0);
        }
    }

    #[test]
    fn backspace_at_line_start_joins_lines() {
        let mut editor = editor_with("abc\ndef");
        editor.cursor_line = 1;

        editor.backspace();

        assert_eq!(editor.buffer.to_string(), "abcdef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));
    }

    #[test]
    fn enter_copies_current_indentation() {
        let mut editor = editor_with("  alpha");
        editor.cursor_col = 7;

        editor.insert_newline();

        assert_eq!(editor.buffer.to_string(), "  alpha\n  ");
        assert_eq!((editor.cursor_line, editor.cursor_col), (1, 2));
    }

    #[test]
    fn tab_advances_to_configured_stop_in_one_edit() {
        let mut editor = editor_with("alpha");
        editor.tab_width = 2;

        editor.insert_tab();

        assert_eq!(editor.buffer.to_string(), "  alpha");
        assert_eq!(editor.edit_count, 1);
    }

    #[test]
    fn delete_at_eol_removes_whole_crlf_separator() {
        let mut editor = editor_with("abc\r\ndef");
        editor.cursor_col = 3;

        editor.delete_char();

        assert_eq!(editor.buffer.to_string(), "abcdef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));
    }

    #[test]
    fn kill_at_eol_keeps_separator_in_kill_ring() {
        let mut editor = editor_with("abc\ndef");
        editor.cursor_col = 3;

        editor.kill_to_eol();

        assert_eq!(editor.buffer.to_string(), "abcdef");
        assert_eq!(editor.kill_ring, "\n");
    }

    #[test]
    fn ctrl_w_kills_active_region() {
        let mut editor = editor_with("abc def");
        editor.cursor_col = 1;
        editor.toggle_mark();
        editor.cursor_col = 5;

        editor.kill_region();

        assert_eq!(editor.buffer.to_string(), "aef");
        assert_eq!(editor.kill_ring, "bc d");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 1));
        assert_eq!(editor.mark, None);
    }

    #[test]
    fn alt_w_copies_active_region_without_deleting() {
        let mut editor = editor_with("abc def");
        editor.cursor_col = 1;
        editor.toggle_mark();
        editor.cursor_col = 5;

        editor.copy_region();

        assert_eq!(editor.buffer.to_string(), "abc def");
        assert_eq!(editor.kill_ring, "bc d");
        assert!(editor.mark.is_some());
    }

    #[test]
    fn yank_replaces_active_region() {
        let mut editor = editor_with("abc def");
        editor.kill_ring = "XYZ".to_string();
        editor.cursor_col = 1;
        editor.toggle_mark();
        editor.cursor_col = 5;

        editor.yank();

        assert_eq!(editor.buffer.to_string(), "aXYZef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));
        assert_eq!(editor.mark, None);
    }

    #[test]
    fn undo_and_redo_restore_text_and_cursor() {
        let mut editor = editor_with("abc");
        editor.cursor_col = 3;

        editor.insert_char('d');
        assert_eq!(editor.buffer.to_string(), "abcd");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));

        editor.undo();
        assert_eq!(editor.buffer.to_string(), "abc");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));

        editor.redo();
        assert_eq!(editor.buffer.to_string(), "abcd");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));
    }

    #[test]
    fn yank_undo_is_single_step() {
        let mut editor = editor_with("abef");
        editor.kill_ring = "cd".to_string();
        editor.cursor_col = 2;

        editor.yank();
        assert_eq!(editor.buffer.to_string(), "abcdef");

        editor.undo();
        assert_eq!(editor.buffer.to_string(), "abef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 2));
    }

    #[test]
    fn fill_paragraph_wraps_current_paragraph() {
        let mut editor = editor_with(
            "before\n\none two three four five six seven eight nine ten eleven twelve\ncontinued here\n\nafter",
        );
        editor.cursor_line = 2;

        editor.fill_paragraph(24);

        assert_eq!(
            editor.buffer.to_string(),
            "before\n\none two three four five\nsix seven eight nine ten\neleven twelve continued\nhere\n\nafter"
        );
        assert_eq!(editor.status, "filled paragraph to 24");
    }

    #[test]
    fn fill_paragraph_preserves_common_indent() {
        let mut editor = editor_with("    alpha beta gamma delta epsilon\n    zeta eta theta");

        editor.fill_paragraph(22);

        assert_eq!(
            editor.buffer.to_string(),
            "    alpha beta gamma\n    delta epsilon zeta\n    eta theta"
        );
    }

    #[test]
    fn fill_paragraph_is_one_undo_step() {
        let mut editor = editor_with("one two three four five");

        editor.fill_paragraph(12);
        assert_eq!(editor.buffer.to_string(), "one two\nthree four\nfive");

        editor.undo();
        assert_eq!(editor.buffer.to_string(), "one two three four five");
    }

    #[test]
    fn down_arrow_after_mark_does_not_exit() {
        let mut editor = editor_with("abc\ndef");
        editor.toggle_mark();
        editor.move_down();

        assert_eq!((editor.cursor_line, editor.cursor_col), (1, 0));
        assert!(editor.active_region().is_some());
    }

    #[test]
    fn ctrl_g_cancels_active_mark() {
        let mut editor = editor_with("abc\ndef");
        editor.toggle_mark();
        editor.move_down();

        editor.cancel_mark();

        assert_eq!(editor.mark, None);
        assert!(editor.active_region().is_none());
        assert_eq!(editor.status, "mark cancelled");
    }

    #[test]
    fn view_mode_esc_and_q_quit() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        assert!(is_view_quit_key(Mode::View, esc));

        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        assert!(is_view_quit_key(Mode::View, q));
    }

    #[test]
    fn edit_mode_esc_and_q_do_not_quit() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        assert!(!is_view_quit_key(Mode::Edit, esc));

        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        assert!(!is_view_quit_key(Mode::Edit, q));
    }

    #[test]
    fn selection_range_after_region_does_not_underflow() {
        let mut editor = editor_with("abc\ndef\nghi");
        editor.toggle_mark();
        editor.move_down();

        assert_eq!(
            line_selection_range(&editor, 2, editor.active_region().as_ref()),
            None
        );
    }

    #[test]
    fn repeated_forward_search_jumps_to_next_match() {
        let mut editor = editor_with("foo bar foo");

        editor.begin_search(SearchDirection::Forward);
        for ch in "foo".chars() {
            editor.search_insert_char(ch);
        }
        editor.search_repeat(SearchDirection::Forward);

        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 8));
    }

    #[test]
    fn repeated_reverse_search_jumps_to_previous_match() {
        let mut editor = editor_with("foo bar foo");
        editor.cursor_col = 11;

        editor.begin_search(SearchDirection::Reverse);
        for ch in "foo".chars() {
            editor.search_insert_char(ch);
        }
        editor.search_repeat(SearchDirection::Reverse);

        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 0));
    }

    #[test]
    fn ctrl_x_exit_chord_works_while_searching() {
        let mut editor = editor_with("abc");
        editor.begin_search(SearchDirection::Forward);

        let start = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(handle_search_key(&mut editor, start).unwrap(), None);
        assert!(editor.ctrl_x_pending);

        let exit = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            handle_ctrl_x_chord(&mut editor, exit, Mode::Edit).unwrap(),
            Some(Outcome::Unchanged)
        );
        assert!(!editor.ctrl_x_pending);
        assert!(editor.search.is_some());
    }

    #[test]
    fn ctrl_g_cancels_incremental_search() {
        let mut editor = editor_with("abc foo");
        editor.cursor_col = 2;
        editor.begin_search(SearchDirection::Forward);
        for ch in "foo".chars() {
            editor.search_insert_char(ch);
        }
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));

        let cancel = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(handle_search_key(&mut editor, cancel).unwrap(), None);

        assert!(editor.search.is_none());
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 2));
    }

    #[test]
    fn dirty_editor_quit_is_discarded() {
        let mut editor = editor_with("abc");
        editor.insert_char('x');

        assert_eq!(editor.quit_outcome(Mode::Edit), Outcome::Discarded);
        assert_eq!(editor.quit_outcome(Mode::Edit).exit_code(), 3);
    }

    #[test]
    fn saved_editor_quit_is_saved() {
        let mut editor = editor_with("abc");
        editor.save_count = 1;

        assert_eq!(editor.quit_outcome(Mode::Edit), Outcome::Saved);
        assert_eq!(editor.quit_outcome(Mode::Edit).exit_code(), 0);
    }

    #[test]
    fn view_quit_is_a_successful_close() {
        let editor = editor_with("abc");

        assert_eq!(editor.quit_outcome(Mode::View), Outcome::Closed);
        assert_eq!(editor.quit_outcome(Mode::View).exit_code(), 0);
    }

    #[test]
    fn config_accepts_stdin_for_inpage() {
        let invocation = cli_args().into_invocation(Mode::View).unwrap();

        assert_eq!(invocation.config.input_path, None);
        assert!(invocation.result_json);
    }

    #[test]
    fn config_rejects_stdin_for_inmacs_until_output_is_supported() {
        let error = cli_args().into_invocation(Mode::Edit).unwrap_err();

        assert!(error.to_string().contains("requires --output"));
    }

    #[test]
    fn config_accepts_stdin_editor_with_explicit_output() {
        let mut args = cli_args();
        args.output = Some(PathBuf::from("result.trash"));
        args.syntax = Some("trashtalk".to_string());

        let invocation = args.into_invocation(Mode::Edit).unwrap();

        assert_eq!(
            invocation.config.output_path,
            Some(PathBuf::from("result.trash"))
        );
        assert_eq!(invocation.config.tab_width, 2);
    }

    #[test]
    fn plus_line_argument_is_preserved_through_clap_normalization() {
        let normalized = normalize_plus_line_args(
            ["inpage", "+12", "source.trash"].map(std::ffi::OsString::from),
        );

        assert_eq!(
            normalized,
            ["inpage", "--line", "12", "source.trash"].map(std::ffi::OsString::from)
        );
    }

    #[test]
    fn annotation_document_requires_supported_schema() {
        let scratch = ScratchDir::new("annotations");
        let path = scratch.join("annotations.json");
        fs::write(
            &path,
            r#"{"schema_version":1,"annotations":[{"line":3,"severity":"error","message":"expected ]"}]}"#,
        )
        .unwrap();

        let document = AnnotationDocument::read(&path).unwrap();

        assert_eq!(document.annotations[0].column, 1);
        assert_eq!(document.annotations[0].line, 3);
    }

    #[test]
    fn trash_extension_selects_bundled_trashtalk_syntax() {
        let highlighter = SyntaxHighlighter::new(None, Some(Path::new("Counter.trash"))).unwrap();
        let config = Config::for_path(Mode::Edit, "Counter.trash");

        assert_eq!(highlighter.syntax_name, "Trashtalk");
        assert_eq!(config.tab_width, 2);
    }

    #[test]
    fn save_requires_confirmation_after_external_change() {
        let scratch = ScratchDir::new("conflict");
        let path = scratch.join("source.trash");
        fs::write(&path, "alpha\n").unwrap();
        let config = Config::for_path(Mode::Edit, &path);
        let mut editor = Editor::open(&config).unwrap();
        editor.insert_char('x');
        fs::write(&path, "external\n").unwrap();

        assert!(!editor.save().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "external\n");
        assert!(editor.status.contains("again to overwrite"));

        assert!(editor.save().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "xalpha\n");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_target_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = ScratchDir::new("permissions");
        let path = scratch.join("source.trash");
        fs::write(&path, "alpha\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let config = Config::for_path(Mode::Edit, &path);
        let mut editor = Editor::open(&config).unwrap();
        editor.insert_char('x');

        assert!(editor.save().unwrap());

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn editable_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let scratch = ScratchDir::new("symlink");
        let target = scratch.join("target.trash");
        let link = scratch.join("link.trash");
        fs::write(&target, "alpha\n").unwrap();
        symlink(&target, &link).unwrap();

        let error = Editor::open(&Config::for_path(Mode::Edit, &link))
            .err()
            .expect("editing a symlink should fail");

        assert!(error.to_string().contains("refusing to edit symlink"));
        assert_eq!(fs::read_to_string(target).unwrap(), "alpha\n");
    }
}
