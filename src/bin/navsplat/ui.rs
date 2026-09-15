use std::path::{Path, PathBuf};

use navsplat::lsp::{LocationHit, Symbol};
use navsplat::preview::PreviewCache;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use tui_spinner::{FluxFrames, FluxSpinner, Spin};

use super::{
    App, FocusArea, SidePaneMode, SidePaneState, active_symbol, current_side_key,
    input_display_value, selected_side_hit,
};

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App) {
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

/// What the preview pane shows: the side pane's hit while that pane has focus,
/// otherwise the active symbol.
struct PreviewTarget {
    file: PathBuf,
    line: u32,
    highlight_start: u32,
    highlight_end: u32,
}

fn preview_target(app: &App) -> Option<PreviewTarget> {
    if app.focus == FocusArea::SidePane
        && let Some(hit) = selected_side_hit(app)
    {
        return Some(PreviewTarget {
            file: hit.file.clone(),
            line: hit.line,
            highlight_start: hit.line,
            highlight_end: hit.line,
        });
    }

    let symbol = active_symbol(app)?;
    let (highlight_start, highlight_end) = if symbol.kind.is_broad_container() {
        (symbol.line, symbol.line)
    } else {
        (
            symbol.line.min(symbol.end_line),
            symbol.line.max(symbol.end_line),
        )
    };
    Some(PreviewTarget {
        file: symbol.file.clone(),
        line: symbol.line,
        highlight_start,
        highlight_end,
    })
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let (lines, title) = match preview_target(app) {
        Some(target) => (
            preview_file_lines(
                &app.root,
                &mut app.preview,
                &target,
                area,
                app.preview_scroll,
            ),
            format!(" Preview {}:{} ", target.file.display(), target.line),
        ),
        None => (
            vec![Line::from("No symbol selected")],
            " Preview ".to_string(),
        ),
    };

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
                .split(area.inner(Margin {
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

fn preview_file_lines(
    root: &Path,
    cache: &mut PreviewCache,
    target: &PreviewTarget,
    area: Rect,
    preview_scroll: isize,
) -> Vec<Line<'static>> {
    let path = if target.file.is_absolute() {
        target.file.clone()
    } else {
        root.join(&target.file)
    };
    let Some(lines) = cache.lines(&path) else {
        return vec![Line::from(format!("Unable to read {}", path.display()))];
    };
    if lines.is_empty() {
        return vec![Line::from(format!("{} is empty", path.display()))];
    }

    let visible_lines = usize::from(area.height.saturating_sub(2)).max(1);
    let first_highlight = target.highlight_start.saturating_sub(1) as isize;
    let default_start = first_highlight - 2;
    let max_start = lines.len().saturating_sub(visible_lines) as isize;
    let start = (default_start + preview_scroll).clamp(0, max_start).max(0) as usize;
    let end = (start + visible_lines).min(lines.len());
    let highlight_start = target.highlight_start as usize;
    let highlight_end = target.highlight_end as usize;

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
            Span::styled(text.clone(), style),
        ]));
    }
    out
}
