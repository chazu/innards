mod lsp;

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use lsp::{CallDirection, LocationHit, LspClient, LspEvent, Symbol, SymbolKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use tui_input::backend::crossterm::to_input_request;
use tui_input::{Input, InputRequest};
use tui_spinner::{FluxFrames, FluxSpinner, Spin};

const DEBOUNCE: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
enum SidePaneMode {
    Source,
    References,
    Callers,
    Callees,
}

impl SidePaneMode {
    fn title(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::References => "References",
            Self::Callers => "Callers",
            Self::Callees => "Callees",
        }
    }

    fn loading_label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::References => "references",
            Self::Callers => "callers",
            Self::Callees => "callees",
        }
    }
}

enum SidePaneState {
    Loading,
    Ready(Vec<LocationHit>),
    Error(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FocusArea {
    Symbols,
    SidePane,
}

#[derive(Debug, Clone)]
struct OpenTarget {
    file: PathBuf,
    line: u32,
}

#[derive(Debug, Clone)]
struct NavigationFrame {
    symbol: Symbol,
    mode: SidePaneMode,
    hits: Vec<LocationHit>,
    selected: usize,
}

#[derive(Debug, Parser)]
#[command(about = "Rust workspace symbol picker powered by rust-analyzer")]
struct Cli {
    #[arg(
        long,
        short,
        help = "Workspace root. Defaults to nearest Cargo.toml or .git ancestor"
    )]
    root: Option<PathBuf>,

    #[arg(
        long,
        help = "Editor command. Defaults to $VISUAL, then $EDITOR, then vi"
    )]
    editor: Option<String>,

    #[arg(
        long,
        default_value_t = 20,
        help = "Inline picker height in terminal rows"
    )]
    height: u16,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(about = "Open the interactive picker")]
    Pick {
        #[arg(help = "Initial query")]
        query: Option<String>,
    },
    #[command(about = "Print workspace symbol matches without opening the TUI")]
    Symbols {
        #[arg(help = "Query to send to rust-analyzer workspace/symbol")]
        query: String,
    },
}

struct App {
    root: PathBuf,
    client: LspClient,
    events: Receiver<LspEvent>,
    input: Input,
    symbols: Vec<Symbol>,
    promoted_symbol: Option<Symbol>,
    navigation_stack: Vec<NavigationFrame>,
    selected: usize,
    preview_scroll: isize,
    dirty_since: Option<Instant>,
    last_sent_query: String,
    last_request_id: i64,
    empty_retry_count: u8,
    status: String,
    loading: bool,
    progress_tokens: HashSet<String>,
    completions_ready: bool,
    side_mode: SidePaneMode,
    side_states: HashMap<String, SidePaneState>,
    side_selected: HashMap<String, usize>,
    focus: FocusArea,
    tick: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = match cli.root {
        Some(root) => root.canonicalize()?,
        None => detect_workspace_root(env::current_dir()?)?,
    };

    let command = cli.command.unwrap_or(Commands::Pick { query: None });
    match command {
        Commands::Pick { query } => {
            run_picker(root, cli.editor, cli.height, query.unwrap_or_default())
        }
        Commands::Symbols { query } => run_symbols(root, query),
    }
}

fn run_symbols(root: PathBuf, query: String) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (client, mut child) = LspClient::start(root.clone(), tx)?;
    lsp::wait_for_ready(&rx, Duration::from_secs(20))?;
    let mut request_id = client.workspace_symbol(query.clone())?;
    let mut progress_tokens = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(20);

    while Instant::now() < deadline {
        let Ok(event) = rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        match event {
            LspEvent::Symbols {
                request_id: id,
                symbols,
                ..
            } if id == request_id && (!symbols.is_empty() || progress_tokens.is_empty()) => {
                for symbol in symbols {
                    println!(
                        "{}:{}:{}\t{} [{}]",
                        symbol.file.display(),
                        symbol.line,
                        symbol.column,
                        symbol.name,
                        symbol.kind.label()
                    );
                }
                client.shutdown();
                let _ = child.wait();
                return Ok(());
            }
            LspEvent::Symbols { request_id: id, .. } if id == request_id => {}
            LspEvent::Progress { token, active, .. } => {
                let was_active = !progress_tokens.is_empty();
                if active {
                    progress_tokens.insert(token);
                } else {
                    progress_tokens.remove(&token);
                }
                if was_active && progress_tokens.is_empty() {
                    request_id = client.workspace_symbol(query.clone())?;
                }
            }
            LspEvent::Error(err) => return Err(anyhow!(err)),
            _ => {}
        }
    }

    client.shutdown();
    let _ = child.wait();
    Err(anyhow!("timed out waiting for workspace/symbol response"))
}

fn run_picker(
    root: PathBuf,
    editor: Option<String>,
    height: u16,
    initial_query: String,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (client, mut child) = LspClient::start(root.clone(), tx)?;
    lsp::wait_for_ready(&rx, Duration::from_secs(20))?;

    let mut terminal = TerminalGuard::enter(height)?;
    let mut app = App {
        root,
        client,
        events: rx,
        input: Input::from(initial_query),
        symbols: Vec::new(),
        promoted_symbol: None,
        navigation_stack: Vec::new(),
        selected: 0,
        preview_scroll: 0,
        dirty_since: Some(Instant::now() - DEBOUNCE),
        last_sent_query: String::new(),
        last_request_id: -1,
        empty_retry_count: 0,
        status: "type to search, enter to open, esc to quit".to_string(),
        loading: false,
        progress_tokens: HashSet::new(),
        completions_ready: false,
        side_mode: SidePaneMode::Source,
        side_states: HashMap::new(),
        side_selected: HashMap::new(),
        focus: FocusArea::Symbols,
        tick: 0,
    };

    let selected = run_event_loop(&mut terminal.terminal, &mut app)?;
    drop(terminal);

    app.client.shutdown();
    let _ = child.wait();

    if let Some(target) = selected {
        open_in_editor(&editor_command(editor), &app.root, &target)?;
    }

    Ok(())
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<Option<OpenTarget>> {
    loop {
        drain_lsp_events(app);
        maybe_send_query(app);
        maybe_request_side_pane(app);
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(result) = handle_key(app, key) {
                        return Ok(result);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<Option<OpenTarget>> {
    match key.code {
        KeyCode::Esc => return Some(None),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Some(None),
        KeyCode::Char('q') if app.input.value().is_empty() => return Some(None),
        KeyCode::Enter => {
            if let Some(result) = handle_enter(app) {
                return Some(result);
            }
        }
        KeyCode::Backspace if app.promoted_symbol.is_some() => pop_navigation(app),
        KeyCode::Tab => toggle_focus(app),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::ALT) => {
            set_side_mode(app, SidePaneMode::References)
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::ALT) => {
            set_side_mode(app, SidePaneMode::Callers)
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::ALT) => {
            set_side_mode(app, SidePaneMode::Callees)
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::ALT) => {
            set_side_mode(app, SidePaneMode::Source)
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::ALT) => copy_selection(app),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => select_prev(app),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => select_next(app),
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => scroll_preview(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => scroll_preview(app, 1),
        KeyCode::Up => select_prev(app),
        KeyCode::Down => select_next(app),
        KeyCode::PageUp => select_delta(app, -10),
        KeyCode::PageDown => select_delta(app, 10),
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL) && app.promoted_symbol.is_none() =>
        {
            handle_input_request(app, InputRequest::DeleteNextChar);
        }
        _ => handle_input_key(app, key),
    }
    None
}

fn handle_enter(app: &mut App) -> Option<Option<OpenTarget>> {
    if app.focus == FocusArea::SidePane {
        let hit = selected_side_hit(app)?.clone();
        if let Some(frame) = current_navigation_frame(app) {
            app.navigation_stack.push(frame);
        }
        app.promoted_symbol = Some(symbol_from_hit(&hit));
        app.focus = FocusArea::Symbols;
        app.preview_scroll = 0;
        app.side_states.clear();
        app.side_selected.clear();
        app.status = format!("promoted {}:{}", hit.file.display(), hit.line);
        return None;
    }

    Some(selected_open_target(app))
}

fn pop_navigation(app: &mut App) {
    if let Some(frame) = app.navigation_stack.pop() {
        let key = side_key(frame.mode, &frame.symbol);
        app.promoted_symbol = Some(frame.symbol);
        app.side_mode = frame.mode;
        app.side_states.clear();
        app.side_selected.clear();
        app.side_states
            .insert(key.clone(), SidePaneState::Ready(frame.hits));
        app.side_selected.insert(key, frame.selected);
        app.focus = FocusArea::SidePane;
        app.status = "popped to previous selection".to_string();
    } else {
        app.promoted_symbol = None;
        app.focus = FocusArea::Symbols;
        app.side_states.clear();
        app.side_selected.clear();
        app.status = format!("{} match(es)", app.symbols.len());
    }
    app.preview_scroll = 0;
}

fn select_prev(app: &mut App) {
    if app.focus == FocusArea::SidePane {
        select_side_delta(app, -1);
        return;
    }

    if app.promoted_symbol.is_some() {
        return;
    }

    let previous = app.selected;
    app.selected = app.selected.saturating_sub(1);
    if app.selected != previous {
        app.preview_scroll = 0;
    }
}

fn select_next(app: &mut App) {
    if app.focus == FocusArea::SidePane {
        select_side_delta(app, 1);
        return;
    }

    if app.promoted_symbol.is_some() {
        return;
    }

    if !app.symbols.is_empty() {
        let previous = app.selected;
        app.selected = (app.selected + 1).min(app.symbols.len() - 1);
        if app.selected != previous {
            app.preview_scroll = 0;
        }
    }
}

fn select_delta(app: &mut App, delta: isize) {
    if app.focus == FocusArea::SidePane {
        select_side_delta(app, delta);
        return;
    }

    if app.promoted_symbol.is_some() {
        return;
    }

    if app.symbols.is_empty() {
        app.selected = 0;
        return;
    }
    if delta.is_negative() {
        app.selected = app.selected.saturating_sub(delta.unsigned_abs());
    } else {
        app.selected = (app.selected + delta as usize).min(app.symbols.len() - 1);
    }
    app.preview_scroll = 0;
}

fn set_side_mode(app: &mut App, mode: SidePaneMode) {
    if app.side_mode != mode {
        app.side_mode = mode;
        app.preview_scroll = 0;
        if mode == SidePaneMode::Source {
            app.focus = FocusArea::Symbols;
        }
    }
}

fn toggle_focus(app: &mut App) {
    if app.side_mode == SidePaneMode::Source {
        return;
    }

    app.focus = match app.focus {
        FocusArea::Symbols if selected_side_hit_count(app) > 0 => FocusArea::SidePane,
        FocusArea::Symbols => FocusArea::Symbols,
        FocusArea::SidePane => FocusArea::Symbols,
    };
    app.preview_scroll = 0;
}

fn select_side_delta(app: &mut App, delta: isize) {
    let Some(key) = current_side_key(app) else {
        app.focus = FocusArea::Symbols;
        return;
    };
    let count = selected_side_hit_count(app);
    if count == 0 {
        app.focus = FocusArea::Symbols;
        return;
    }

    let selected = app.side_selected.entry(key).or_insert(0);
    let previous = *selected;
    if delta.is_negative() {
        *selected = selected.saturating_sub(delta.unsigned_abs());
    } else {
        *selected = (*selected + delta as usize).min(count - 1);
    }
    if *selected != previous {
        app.preview_scroll = 0;
    }
}

fn scroll_preview(app: &mut App, delta: isize) {
    if !app.symbols.is_empty() {
        app.preview_scroll = app.preview_scroll.saturating_add(delta);
    }
}

fn handle_input_key(app: &mut App, key: KeyEvent) {
    if app.promoted_symbol.is_some() {
        return;
    }

    let event = Event::Key(key);
    if let Some(request) = to_input_request(&event) {
        handle_input_request(app, request);
    }
}

fn handle_input_request(app: &mut App, request: InputRequest) {
    if let Some(changed) = app.input.handle(request) {
        if changed.value {
            mark_dirty(app);
        }
    }
}

fn mark_dirty(app: &mut App) {
    app.dirty_since = Some(Instant::now());
    app.status = "searching...".to_string();
    app.loading = true;
    app.empty_retry_count = 0;
    app.promoted_symbol = None;
    app.navigation_stack.clear();
}

fn maybe_send_query(app: &mut App) {
    let Some(dirty_since) = app.dirty_since else {
        return;
    };
    let query = app.input.value();
    if dirty_since.elapsed() < DEBOUNCE || query == app.last_sent_query {
        return;
    }

    match app.client.workspace_symbol(query.to_string()) {
        Ok(request_id) => {
            app.last_request_id = request_id;
            app.last_sent_query = query.to_string();
            app.dirty_since = None;
            app.loading = true;
            app.completions_ready = false;
        }
        Err(err) => {
            app.status = err.to_string();
            app.dirty_since = None;
            app.loading = false;
        }
    }
}

fn schedule_empty_retry(app: &mut App) {
    app.empty_retry_count = app.empty_retry_count.saturating_add(1);
    app.status = "rust-analyzer warming up; retrying search...".to_string();
    app.loading = true;
    app.completions_ready = false;
    app.dirty_since = Some(Instant::now());
    app.last_sent_query.clear();
}

fn maybe_request_side_pane(app: &mut App) {
    if !app.completions_ready || app.side_mode == SidePaneMode::Source {
        return;
    }

    let Some(symbol) = active_symbol(app).cloned() else {
        return;
    };
    let key = side_key(app.side_mode, &symbol);
    if app.side_states.contains_key(&key) {
        return;
    }

    app.side_states.insert(key.clone(), SidePaneState::Loading);
    let result = match app.side_mode {
        SidePaneMode::Source => return,
        SidePaneMode::References => app.client.references(&symbol, key.clone()),
        SidePaneMode::Callers => app.client.incoming_calls(&symbol, key.clone()),
        SidePaneMode::Callees => app.client.outgoing_calls(&symbol, key.clone()),
    };

    match result {
        Ok(_) => {
            app.status = format!("loading {}...", app.side_mode.loading_label());
            app.loading = true;
        }
        Err(err) => {
            app.side_states
                .insert(key, SidePaneState::Error(err.to_string()));
            app.status = err.to_string();
            app.loading = false;
        }
    }
}

fn side_key(mode: SidePaneMode, symbol: &Symbol) -> String {
    format!(
        "{mode:?}\t{}\t{}\t{}\t{}",
        symbol.file.display(),
        symbol.line,
        symbol.column,
        symbol.name
    )
}

fn current_side_key(app: &App) -> Option<String> {
    let symbol = active_symbol(app)?;
    (app.side_mode != SidePaneMode::Source).then(|| side_key(app.side_mode, symbol))
}

fn active_symbol(app: &App) -> Option<&Symbol> {
    app.promoted_symbol
        .as_ref()
        .or_else(|| app.symbols.get(app.selected))
}

fn selected_side_hit_count(app: &App) -> usize {
    let Some(key) = current_side_key(app) else {
        return 0;
    };
    match app.side_states.get(&key) {
        Some(SidePaneState::Ready(hits)) => hits.len(),
        _ => 0,
    }
}

fn selected_side_hit(app: &App) -> Option<&LocationHit> {
    let key = current_side_key(app)?;
    let selected = app.side_selected.get(&key).copied().unwrap_or(0);
    match app.side_states.get(&key) {
        Some(SidePaneState::Ready(hits)) => hits.get(selected),
        _ => None,
    }
}

fn current_navigation_frame(app: &App) -> Option<NavigationFrame> {
    let symbol = active_symbol(app)?.clone();
    let key = current_side_key(app)?;
    let selected = app.side_selected.get(&key).copied().unwrap_or(0);
    let hits = match app.side_states.get(&key) {
        Some(SidePaneState::Ready(hits)) => hits.clone(),
        _ => return None,
    };
    Some(NavigationFrame {
        symbol,
        mode: app.side_mode,
        hits,
        selected,
    })
}

fn selected_open_target(app: &App) -> Option<OpenTarget> {
    if app.focus == FocusArea::SidePane {
        if let Some(hit) = selected_side_hit(app) {
            return Some(OpenTarget {
                file: hit.file.clone(),
                line: hit.line,
            });
        }
    }

    active_symbol(app).map(|symbol| OpenTarget {
        file: symbol.file.clone(),
        line: symbol.line,
    })
}

fn selected_clipboard_text(app: &App) -> Option<String> {
    if app.focus == FocusArea::SidePane {
        if let Some(hit) = selected_side_hit(app) {
            let name = hit.name.as_deref().unwrap_or("");
            let kind = hit.kind.map(|kind| kind.label()).unwrap_or("Location");
            return Some(format!(
                "{}:{}:{}\t{} [{}]",
                hit.file.display(),
                hit.line,
                hit.column,
                name,
                kind
            ));
        }
    }

    active_symbol(app).map(|symbol| {
        format!(
            "{}:{}:{}\t{} [{}]",
            symbol.file.display(),
            symbol.line,
            symbol.column,
            symbol.name,
            symbol.kind.label()
        )
    })
}

fn copy_selection(app: &mut App) {
    let Some(text) = selected_clipboard_text(app) else {
        app.status = "nothing selected to copy".to_string();
        return;
    };

    match copy_to_clipboard(&text) {
        Ok(tool) => app.status = format!("copied selection with {tool}"),
        Err(err) => app.status = err.to_string(),
    }
}

fn copy_to_clipboard(text: &str) -> Result<String> {
    let commands: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    let mut failures = Vec::new();

    for (program, args) in commands {
        let mut child = match Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                failures.push(format!("{program}: failed to start: {err}"));
                continue;
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(err) = stdin.write_all(text.as_bytes()) {
                failures.push(format!("{program}: failed to write selection: {err}"));
                continue;
            }
        }
        match child.wait_with_output() {
            Ok(output) if output.status.success() => return Ok(program.to_string()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = stderr.trim();
                if message.is_empty() {
                    failures.push(format!("{program}: exited with {}", output.status));
                } else {
                    failures.push(format!("{program}: {message}"));
                }
            }
            Err(err) => failures.push(format!("{program}: failed to wait: {err}")),
        }
    }

    match copy_with_osc52(text) {
        Ok(()) => Ok("OSC 52".to_string()),
        Err(err) if failures.is_empty() => Err(err),
        Err(err) => Err(anyhow!(
            "clipboard tools failed ({}); OSC 52 failed: {err}",
            failures.join("; ")
        )),
    }
}

fn copy_with_osc52(text: &str) -> Result<()> {
    let payload = base64_encode(text.as_bytes());
    let sequence = if env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;\x1b\x1b]52;c;{payload}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{payload}\x07")
    };
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }

    output
}

fn symbol_from_hit(hit: &LocationHit) -> Symbol {
    Symbol {
        name: hit
            .name
            .clone()
            .unwrap_or_else(|| format!("{}:{}", hit.file.display(), hit.line)),
        kind: hit.kind.unwrap_or(SymbolKind::Unknown),
        file: hit.file.clone(),
        line: hit.line,
        end_line: hit.line,
        column: hit.column,
        container_name: hit.detail.clone(),
    }
}

fn drain_lsp_events(app: &mut App) {
    while let Ok(event) = app.events.try_recv() {
        match event {
            LspEvent::Symbols {
                request_id,
                query,
                symbols,
            } if request_id == app.last_request_id && query == app.last_sent_query => {
                if symbols.is_empty()
                    && !query.is_empty()
                    && (!app.progress_tokens.is_empty() || app.empty_retry_count < 5)
                {
                    schedule_empty_retry(app);
                    continue;
                }
                app.symbols = symbols;
                if app.promoted_symbol.is_none() {
                    app.navigation_stack.clear();
                    app.selected = app.selected.min(app.symbols.len().saturating_sub(1));
                    app.preview_scroll = 0;
                    app.side_states.clear();
                    app.side_selected.clear();
                    app.focus = FocusArea::Symbols;
                    if !app.progress_tokens.is_empty() && app.symbols.is_empty() {
                        app.status = "rust-analyzer is still indexing...".to_string();
                        app.loading = true;
                        app.completions_ready = false;
                    } else {
                        app.empty_retry_count = 0;
                        app.status = format!("{} match(es)", app.symbols.len());
                        app.loading = false;
                        app.completions_ready = true;
                    }
                }
            }
            LspEvent::Progress {
                token,
                active,
                message,
            } => {
                let was_active = !app.progress_tokens.is_empty();
                if active {
                    app.progress_tokens.insert(token);
                    app.status = format!("rust-analyzer: {message}");
                    app.loading = true;
                } else {
                    app.progress_tokens.remove(&token);
                    let is_active = !app.progress_tokens.is_empty();
                    app.status = if is_active {
                        "rust-analyzer is indexing...".to_string()
                    } else {
                        "rust-analyzer ready".to_string()
                    };
                    if was_active && !is_active && !app.input.value().is_empty() {
                        app.dirty_since = Some(Instant::now() - DEBOUNCE);
                        app.loading = true;
                    } else {
                        app.loading = is_active;
                    }
                }
            }
            LspEvent::Error(err) => {
                app.status = err;
                app.loading = false;
                app.completions_ready = false;
            }
            LspEvent::References { key, hits } => {
                app.side_selected.insert(key.clone(), 0);
                app.side_states
                    .insert(key.clone(), SidePaneState::Ready(hits));
                if current_side_key(app).as_deref() == Some(key.as_str()) {
                    app.status = "references loaded".to_string();
                    app.loading = !app.progress_tokens.is_empty();
                }
            }
            LspEvent::CallHierarchy {
                key,
                direction,
                hits,
                ..
            } => {
                let label = match direction {
                    CallDirection::Incoming => "callers",
                    CallDirection::Outgoing => "callees",
                };
                app.side_selected.insert(key.clone(), 0);
                app.side_states
                    .insert(key.clone(), SidePaneState::Ready(hits));
                if current_side_key(app).as_deref() == Some(key.as_str()) {
                    app.status = format!("{label} loaded");
                    app.loading = !app.progress_tokens.is_empty();
                }
            }
            LspEvent::SideError { key, message } => {
                app.side_states
                    .insert(key.clone(), SidePaneState::Error(message.clone()));
                if current_side_key(app).as_deref() == Some(key.as_str()) {
                    app.status = message;
                    app.loading = !app.progress_tokens.is_empty();
                }
            }
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    if !app.completions_ready {
        draw_loading(frame, app);
        return;
    }

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let input = Paragraph::new(input_display_value(app))
        .block(Block::default().title(" Navsplat ").borders(Borders::ALL))
        .style(Style::default().fg(Color::White));
    frame.render_widget(input, outer[0]);
    set_input_cursor(frame, outer[0], app);

    if app.side_mode == SidePaneMode::Source {
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(9)])
            .split(outer[1]);
        draw_symbols(frame, body[0], app);
        draw_preview(frame, body[1], app);
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(outer[1]);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(9)])
            .split(panes[0]);
        draw_symbols(frame, left[0], app);
        draw_preview(frame, left[1], app);
        draw_side_pane(frame, panes[1], app);
    }

    draw_bottom_panel(frame, outer[2], app);
}

fn input_display_value(app: &App) -> String {
    if let Some(symbol) = &app.promoted_symbol {
        format!(
            "current: {} [{}] {}:{}  (backspace pops)",
            symbol.name,
            symbol.kind.label(),
            symbol.file.display(),
            symbol.line
        )
    } else {
        app.input.value().to_string()
    }
}

fn set_input_cursor(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let inner_width = area.width.saturating_sub(2);
    let cursor = if app.promoted_symbol.is_some() {
        input_display_value(app).chars().count()
    } else {
        app.input.visual_cursor()
    }
    .min(inner_width as usize) as u16;
    frame.set_cursor_position((area.x + 1 + cursor, area.y + 1));
}

fn draw_loading(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(area);

    let input = Paragraph::new(input_display_value(app))
        .block(Block::default().title(" Navsplat ").borders(Borders::ALL))
        .style(Style::default().fg(Color::White));
    frame.render_widget(input, outer[0]);
    set_input_cursor(frame, outer[0], app);

    draw_bottom_panel(frame, outer[1], app);
}

fn draw_bottom_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    let status_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(rows[0]);
    if app.loading {
        frame.render_widget(Clear, status_area[0]);
        frame.render_widget(
            FluxSpinner::new(app.tick)
                .frames(FluxFrames::CLASSIC)
                .spin(Spin::Clockwise)
                .color(Color::Cyan),
            status_area[0],
        );
    }
    frame.render_widget(Clear, status_area[1]);
    let status = Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, status_area[1]);

    let focus = match app.focus {
        FocusArea::Symbols => "symbols",
        FocusArea::SidePane => "results",
    };
    let pop_hint = if app.promoted_symbol.is_some() {
        " | backspace pop"
    } else {
        ""
    };
    let keys = format!(
        "keys: focus={focus} | enter open/promote | alt-y copy | tab switch focus | up/down move | shift-up/down preview | alt-r refs | alt-c callers | alt-e callees | alt-s source{pop_hint} | esc quit"
    );
    let help = Paragraph::new(keys).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Clear, rows[1]);
    frame.render_widget(help, rows[1]);
}

fn draw_symbols(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let promoted = app.promoted_symbol.as_ref();
    let items: Vec<ListItem<'_>> = promoted
        .into_iter()
        .chain(app.symbols.iter().filter(|_| promoted.is_none()))
        .map(symbol_list_item)
        .collect();

    let border_style = if app.focus == FocusArea::Symbols {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let title = if app.promoted_symbol.is_some() {
        " Current "
    } else {
        " Symbols "
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">");

    let mut state = ListState::default();
    if app.promoted_symbol.is_some() {
        state.select(Some(0));
    } else if !app.symbols.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn symbol_list_item(symbol: &Symbol) -> ListItem<'static> {
    let location = format!("{}:{}", symbol.file.display(), symbol.line);
    let name = if let Some(container) = &symbol.container_name {
        format!("{container}::{}", symbol.name)
    } else {
        symbol.name.clone()
    };
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<12}", symbol.kind.label()),
            Style::default().fg(Color::Blue),
        ),
        Span::raw(" "),
        Span::styled(name, Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(location, Style::default().fg(Color::DarkGray)),
    ]))
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.focus == FocusArea::SidePane {
        if let Some(hit) = selected_side_hit(app) {
            let lines = preview_file_lines(
                &app.root,
                &hit.file,
                hit.line,
                hit.line,
                area,
                app.preview_scroll,
            );
            let title = format!(" Preview {}:{} ", hit.file.display(), hit.line);
            let preview = Paragraph::new(lines)
                .block(Block::default().title(title).borders(Borders::ALL))
                .wrap(Wrap { trim: false });
            frame.render_widget(preview, area);
            return;
        }
    }

    let selected = active_symbol(app);
    let lines = selected
        .map(|symbol| preview_lines(&app.root, symbol, area, app.preview_scroll))
        .unwrap_or_else(|| vec![Line::from("No symbol selected")]);

    let title = selected
        .map(|symbol| format!(" Preview {}:{} ", symbol.file.display(), symbol.line))
        .unwrap_or_else(|| " Preview ".to_string());

    let preview = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(preview, area);
}

fn draw_side_pane(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = format!(" {} ", app.side_mode.title());
    let border_style = if app.focus == FocusArea::SidePane {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let Some(key) = current_side_key(app) else {
        let pane = Paragraph::new("No symbol selected")
            .block(Block::default().title(title).borders(Borders::ALL));
        frame.render_widget(pane, area);
        return;
    };

    match app.side_states.get(&key) {
        Some(SidePaneState::Loading) | None => {
            let inner = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(2), Constraint::Min(1)])
                .split(area.inner(ratatui::layout::Margin {
                    vertical: 1,
                    horizontal: 1,
                }));
            frame.render_widget(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(border_style),
                area,
            );
            frame.render_widget(
                FluxSpinner::new(app.tick)
                    .frames(FluxFrames::CLASSIC)
                    .spin(Spin::Clockwise)
                    .color(Color::Cyan),
                inner[0],
            );
            frame.render_widget(
                Paragraph::new(format!("loading {}...", app.side_mode.loading_label()))
                    .style(Style::default().fg(Color::DarkGray)),
                inner[1],
            );
        }
        Some(SidePaneState::Error(message)) => {
            let pane = Paragraph::new(message.as_str())
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(border_style),
                )
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false });
            frame.render_widget(pane, area);
        }
        Some(SidePaneState::Ready(hits)) => {
            let items = side_pane_items(hits);
            let list = List::new(items)
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(border_style),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(">");

            let selected = app.side_selected.get(&key).copied().unwrap_or(0);
            let mut state = ListState::default();
            if app.focus == FocusArea::SidePane && !hits.is_empty() {
                state.select(Some(selected.min(hits.len() - 1)));
            }
            frame.render_stateful_widget(list, area, &mut state);
        }
    }
}

fn side_pane_items(hits: &[LocationHit]) -> Vec<ListItem<'static>> {
    if hits.is_empty() {
        return vec![ListItem::new(Line::from("No results"))];
    }

    hits.iter()
        .map(|hit| {
            let location = format!("{}:{}:{}", hit.file.display(), hit.line, hit.column);
            let label = hit.name.clone().unwrap_or_default();
            let kind = hit.kind.map(|kind| kind.label()).unwrap_or("");
            let detail = hit.detail.clone().unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(location, Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::styled(kind, Style::default().fg(Color::Blue)),
                Span::raw(" "),
                Span::styled(label, Style::default().fg(Color::White)),
                Span::raw(" "),
                Span::styled(detail, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect()
}

fn preview_lines(
    root: &Path,
    symbol: &Symbol,
    area: Rect,
    preview_scroll: isize,
) -> Vec<Line<'static>> {
    let (highlight_start, highlight_end) = if symbol.kind.is_broad_container() {
        (symbol.line, symbol.line)
    } else {
        (
            symbol.line.min(symbol.end_line),
            symbol.line.max(symbol.end_line),
        )
    };
    preview_file_lines(
        root,
        &symbol.file,
        highlight_start,
        highlight_end,
        area,
        preview_scroll,
    )
}

fn preview_file_lines(
    root: &Path,
    file: &Path,
    highlight_start: u32,
    highlight_end: u32,
    area: Rect,
    preview_scroll: isize,
) -> Vec<Line<'static>> {
    let path = if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![Line::from(format!("Unable to read {}", path.display()))];
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return vec![Line::from(format!("{} is empty", path.display()))];
    }

    let visible_lines = usize::from(area.height.saturating_sub(2)).max(1);
    let target = highlight_start.saturating_sub(1) as isize;
    let default_start = target - 2;
    let max_start = lines.len().saturating_sub(visible_lines) as isize;
    let start = (default_start + preview_scroll).clamp(0, max_start).max(0) as usize;
    let end = (start + visible_lines).min(lines.len());
    let highlight_start = highlight_start as usize;
    let highlight_end = highlight_end as usize;

    let mut out = Vec::with_capacity(end - start);
    for (index, text) in lines[start..end].iter().enumerate() {
        let line_no = start + index + 1;
        let style = if (highlight_start..=highlight_end).contains(&line_no) {
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        out.push(Line::from(vec![
            Span::styled(
                format!("{line_no:>5} "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled((*text).to_string(), style),
        ]));
    }
    out
}

fn open_in_editor(editor: &str, root: &Path, target: &OpenTarget) -> Result<()> {
    let path = if target.file.is_absolute() {
        target.file.clone()
    } else {
        root.join(&target.file)
    };

    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("empty editor command"))?;
    let args: Vec<&str> = parts.collect();

    Command::new(program)
        .args(args)
        .arg(format!("+{}", target.line))
        .arg(path)
        .status()
        .with_context(|| format!("failed to run editor command: {editor}"))?;
    Ok(())
}

fn editor_command(editor: Option<String>) -> String {
    editor
        .or_else(|| env::var("VISUAL").ok())
        .or_else(|| env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_string())
}

fn detect_workspace_root(mut start: PathBuf) -> Result<PathBuf> {
    start = start.canonicalize()?;
    let mut current = Some(start.as_path());
    while let Some(path) = current {
        if path.join("Cargo.toml").exists() || path.join(".git").exists() {
            return Ok(path.to_path_buf());
        }
        current = path.parent();
    }
    Err(anyhow!(
        "could not find workspace root from current directory; pass --root"
    ))
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(10)),
            },
        )?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}
