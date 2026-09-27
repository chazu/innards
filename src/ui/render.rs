use super::{
    model::App,
    protocol::{Kind, Node},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::collections::HashMap;

#[derive(Default)]
pub struct Renderer {
    pub areas: HashMap<String, Rect>,
    text: HashMap<String, (String, u16, Vec<String>)>,
    pub plots: HashMap<String, super::plot::Plot>,
    pub dividers: HashMap<String, Rect>,
    focus: Vec<String>,
}
// Fixed cell wrapping is cached for unchanged text/width. Ratatui supplies
// display widths, including wide characters; byte indexes never become columns.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut rows = vec![String::new()];
    let mut column = 0;
    for ch in text.chars() {
        if ch == '\n' {
            rows.push(String::new());
            column = 0;
            continue;
        }
        let glyph = if ch == '\t' {
            "    ".to_owned()
        } else if ch.is_control() {
            "�".to_owned()
        } else {
            ch.to_string()
        };
        let cells = Line::from(glyph.as_str()).width();
        if column + cells > width && column > 0 {
            rows.push(String::new());
            column = 0;
        }
        rows.last_mut().unwrap().push_str(&glyph);
        column += cells;
    }
    rows
}
impl Renderer {
    pub fn draw(&mut self, app: &mut App, frame: &mut Frame) {
        self.areas.clear();
        self.dividers.clear();
        self.focus.clear();
        self.plots.retain(|key, _| app.nodes.contains_key(key));
        self.text.retain(|key, _| app.nodes.contains_key(key));
        let area = frame.area();
        let body = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        app.profile.gauge("visible_rows", 0);
        if let Some(root) = app.root.take() {
            self.node(app, frame, &root, body);
            app.root = Some(root);
        }
        app.focus_order = self.focus.clone();
        if !app.focus_order.contains(&app.focus) {
            app.focus = app.focus_order.first().cloned().unwrap_or_default();
        }
        let status = if app.status.is_empty() {
            "Tab focus · Enter act · PgUp/PgDn scroll · Esc detach"
        } else {
            &app.status
        };
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        );
    }
    fn cells(frame: &mut Frame, area: Rect, cells: &[super::protocol::Glyph]) {
        for cell in cells {
            if cell.x < area.width && cell.y < area.height {
                let style = Style::default().fg(if cell.style == "error" {
                    Color::Red
                } else {
                    Color::Cyan
                });
                frame.render_widget(
                    Paragraph::new(cell.text.as_str()).style(style),
                    Rect::new(area.x + cell.x, area.y + cell.y, area.width - cell.x, 1),
                );
            }
        }
    }
    fn node(&mut self, app: &mut App, frame: &mut Frame, node: &Node, area: Rect) {
        let p = &node.props;
        if p.hidden || area.width == 0 || area.height == 0 {
            return;
        }
        self.areas.insert(node.key.clone(), area);
        if app.enabled(&node.key)
            && matches!(
                node.kind,
                Kind::Input
                    | Kind::Button
                    | Kind::List
                    | Kind::Table
                    | Kind::Toggle
                    | Kind::Select
                    | Kind::Split
                    | Kind::Plot
            )
        {
            self.focus.push(node.key.clone());
        }
        let focused = app.focus == node.key;
        let mut style = Style::default();
        if p.style == "primary" {
            style = style.fg(Color::Cyan);
        }
        if p.style == "error" {
            style = style.fg(Color::Red);
        }
        if !app.enabled(&node.key) {
            style = style.fg(Color::DarkGray);
        } else if focused {
            style = style.fg(Color::Cyan);
        }
        let mut title = if p.title.is_empty() {
            p.accessibility_label.clone()
        } else {
            p.title.clone()
        };
        if matches!(node.kind, Kind::List | Kind::Table)
            && let Some(state) = app.local.get(&node.key)
            && state.unread > 0
        {
            title.push_str(&format!(" (+{} new)", state.unread));
        }
        let block = Block::default()
            .borders(if p.border {
                Borders::ALL
            } else {
                Borders::NONE
            })
            .title(title)
            .style(style);
        let mut inner = block.inner(area);
        frame.render_widget(block, area);
        let pad = p.padding.min(inner.width / 2).min(inner.height / 2);
        inner = Rect::new(
            inner.x + pad,
            inner.y + pad,
            inner.width - 2 * pad,
            inner.height - 2 * pad,
        );
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        match node.kind {
            Kind::Panel => {
                let children: Vec<_> = node
                    .children
                    .iter()
                    .filter(|n| !n.props.hidden && n.props.min_parent_width <= inner.width)
                    .collect();
                if children.is_empty() {
                    return;
                }
                let direction = if p.direction == "row" {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                };
                let constraints: Vec<_> = children
                    .iter()
                    .map(|n| {
                        if n.props.size > 0 {
                            Constraint::Length(n.props.size)
                        } else {
                            Constraint::Fill(1)
                        }
                    })
                    .collect();
                let started = app.profile.start();
                let areas = Layout::default()
                    .direction(direction)
                    .constraints(constraints)
                    .spacing(p.gap)
                    .split(inner);
                app.profile.end("layout", started);
                for (child, rect) in children.into_iter().zip(areas.iter()) {
                    self.node(app, frame, child, *rect);
                }
            }
            Kind::Text => {
                let text = if p.detail_of.is_empty() {
                    p.text.clone()
                } else {
                    app.selected_row(&p.detail_of)
                        .and_then(|r| {
                            r.fields.get(if p.field.is_empty() {
                                "detail"
                            } else {
                                &p.field
                            })
                        })
                        .cloned()
                        .unwrap_or_else(|| "Loading…".into())
                };
                if self
                    .text
                    .get(&node.key)
                    .is_none_or(|(old, width, _)| old != &text || *width != inner.width)
                {
                    let rows = wrap(&text, inner.width);
                    self.text
                        .insert(node.key.clone(), (text, inner.width, rows));
                    app.profile.count("text_cache_misses", 1);
                }
                let rows = &self.text[&node.key].2;
                frame.render_widget(
                    Paragraph::new(
                        rows.iter()
                            .take(inner.height as usize)
                            .map(|s| Line::from(s.as_str()))
                            .collect::<Vec<_>>(),
                    )
                    .style(style),
                    inner,
                );
            }
            Kind::Button => {
                frame.render_widget(
                    Paragraph::new(format!("[ {} ]", p.text)).style(style.add_modifier(
                        if focused {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        },
                    )),
                    inner,
                );
            }
            Kind::Toggle => {
                let state = &app.local[&node.key];
                frame.render_widget(
                    Paragraph::new(format!(
                        "[{}] {}",
                        if state.draft == "true" { "x" } else { " " },
                        p.text
                    ))
                    .style(style),
                    inner,
                );
            }
            Kind::Select => {
                let state = &app.local[&node.key];
                frame.render_widget(
                    Paragraph::new(format!("‹ {} ›", state.draft)).style(style),
                    inner,
                );
            }
            Kind::Split => {
                let state = &app.local[&node.key];
                let horizontal = p.direction == "row";
                let dimension = if horizontal {
                    inner.width
                } else {
                    inner.height
                };
                let first = dimension.saturating_sub(1) * state.split / 100;
                let areas = Layout::default()
                    .direction(if horizontal {
                        Direction::Horizontal
                    } else {
                        Direction::Vertical
                    })
                    .constraints([
                        Constraint::Length(first),
                        Constraint::Length(1),
                        Constraint::Fill(1),
                    ])
                    .split(inner);
                self.node(app, frame, &node.children[0], areas[0]);
                self.node(app, frame, &node.children[1], areas[2]);
                frame.render_widget(
                    Paragraph::new(if horizontal {
                        (0..areas[1].height)
                            .map(|_| "│")
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        "─".repeat(areas[1].width as usize)
                    })
                    .style(style),
                    areas[1],
                );
                self.dividers.insert(node.key.clone(), areas[1]);
            }
            Kind::Canvas => {
                Self::cells(frame, inner, &p.cells);
            }
            Kind::Plot => {
                let Some(data) = app.data.get(&p.source) else {
                    return;
                };
                let state = &app.local[&node.key];
                let height = inner.height.saturating_sub(1);
                let domain = state.plot_domain;
                if self.plots.get(&node.key).is_none_or(|cache| {
                    cache.revision != data.generation
                        || cache.width != inner.width
                        || cache.height != height
                        || cache.domain != domain
                        || cache.config != (p.x_field.clone(), p.y_field.clone(), p.domain_y)
                }) {
                    let mut plot = super::plot::Plot::build(data, p, inner.width, height, domain);
                    plot.revision = data.generation;
                    self.plots.insert(node.key.clone(), plot);
                    app.profile.count("plot_cache_misses", 1);
                }
                let plot = &self.plots[&node.key];
                Self::cells(frame, Rect { height, ..inner }, &plot.cells);
                let tooltip = state
                    .plot_cursor
                    .and_then(|x| plot.columns.get(x))
                    .and_then(Option::as_ref)
                    .map(|s| format!("{}: x {:.2}, y {:.2}", s.key, s.x, s.y));
                frame.render_widget(
                    Paragraph::new(tooltip.as_deref().unwrap_or(&plot.label)).style(style),
                    Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
                );
            }
            Kind::Input => {
                let state = &app.local[&node.key];
                let text = if state.draft.is_empty() {
                    &p.placeholder
                } else {
                    &state.draft
                };
                if self
                    .text
                    .get(&node.key)
                    .is_none_or(|(old, width, _)| old != text || *width != inner.width)
                {
                    self.text.insert(
                        node.key.clone(),
                        (text.clone(), inner.width, wrap(text, inner.width)),
                    );
                }
                let before = wrap(&state.draft[..state.cursor], inner.width);
                let mut y = before.len().saturating_sub(1);
                let mut x = Line::from(before.last().unwrap().as_str()).width();
                if x >= inner.width as usize {
                    x = 0;
                    y += 1;
                }
                let top = y.saturating_sub(inner.height.saturating_sub(1) as usize);
                let rows = &self.text[&node.key].2;
                frame.render_widget(
                    Paragraph::new(
                        rows.iter()
                            .skip(top)
                            .take(inner.height as usize)
                            .map(|s| Line::from(s.as_str()))
                            .collect::<Vec<_>>(),
                    )
                    .style(style),
                    inner,
                );
                if focused {
                    frame.set_cursor_position((inner.x + x as u16, inner.y + (y - top) as u16));
                }
            }
            Kind::List | Kind::Table => {
                let height = inner
                    .height
                    .saturating_sub(u16::from(node.kind == Kind::Table))
                    as usize;
                app.visible.insert(node.key.clone(), height);
                let Some(data) = app.data.get(&p.source) else {
                    return;
                };
                let state = app.local.get_mut(&node.key).unwrap();
                if state.following {
                    state.offset = data.total.saturating_sub(height);
                    state.selected = data.total.saturating_sub(1);
                    state.unread = 0;
                }
                state.offset = state.offset.min(data.total.saturating_sub(height));
                state.selected = state.selected.min(data.total.saturating_sub(1));
                let end = (state.offset + height).min(data.total);
                app.profile.count("rows_rendered", end - state.offset);
                let previous = app
                    .profile
                    .counters
                    .get("visible_rows")
                    .copied()
                    .unwrap_or(0);
                app.profile
                    .gauge("visible_rows", previous as usize + end - state.offset);
                if node.kind == Kind::Table {
                    let columns = if p.columns.is_empty() {
                        vec!["text".into()]
                    } else {
                        p.columns.clone()
                    };
                    let rows = (state.offset..end)
                        .map(|index| {
                            let cells = columns
                                .iter()
                                .map(|field| {
                                    Cell::from(
                                        data.row(index)
                                            .and_then(|r| r.fields.get(field))
                                            .map(String::as_str)
                                            .unwrap_or("…"),
                                    )
                                })
                                .collect::<Vec<_>>();
                            Row::new(cells).style(if index == state.selected {
                                style.bg(Color::DarkGray)
                            } else {
                                style
                            })
                        })
                        .collect::<Vec<_>>();
                    frame.render_widget(
                        Table::new(rows, vec![Constraint::Fill(1); columns.len()])
                            .header(Row::new(columns).style(style.add_modifier(Modifier::BOLD))),
                        inner,
                    );
                    return;
                }
                for (y, index) in (state.offset..end).enumerate() {
                    let row = data.row(index);
                    let text = row
                        .and_then(|r| {
                            r.fields
                                .get(if p.field.is_empty() { "text" } else { &p.field })
                        })
                        .map(String::as_str)
                        .unwrap_or("Loading…");
                    let row_style = if index == state.selected {
                        style.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    } else {
                        style
                    };
                    frame.render_widget(
                        Paragraph::new(text).style(row_style),
                        Rect::new(inner.x, inner.y + y as u16, inner.width, 1),
                    );
                }
            }
        }
    }
}
