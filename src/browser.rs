//! Read-only Smalltalk-style browser: an index from Trashtalk in, terminal UI out.
use crate::inline_terminal::{InlineTerminal, termination_flag};
use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Index {
    pub schema_version: u8,
    pub root: String,
    pub classes: Vec<Class>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Class {
    pub name: String,
    pub package: String,
    pub kind: String,
    pub superclass: String,
    pub path: String,
    #[serde(default)]
    pub methods: Vec<Method>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Method {
    pub selector: String,
    pub side: String,
    pub kind: String,
    pub raw: bool,
    pub line: usize,
}
#[derive(Clone, Copy)]
enum Pane {
    Packages,
    Classes,
    Protocols,
    Methods,
}
pub struct App {
    index: Index,
    pane: Pane,
    package: usize,
    class: usize,
    protocol: usize,
    method: usize,
    source_scroll: usize,
}

pub fn parse(input: &str) -> Result<Index> {
    let x: Index = serde_json::from_str(input)?;
    if x.schema_version != 1
        || x.root.is_empty()
        || x.classes
            .iter()
            .any(|c| c.name.is_empty() || c.path.is_empty())
    {
        bail!("Invalid browser index")
    };
    Ok(x)
}
impl App {
    pub fn new(mut index: Index) -> Self {
        index.classes.sort_by(|a, b| {
            (a.package.as_str(), a.name.as_str()).cmp(&(b.package.as_str(), b.name.as_str()))
        });
        Self {
            index,
            pane: Pane::Packages,
            package: 0,
            class: 0,
            protocol: 0,
            method: 0,
            source_scroll: 0,
        }
    }
    fn packages(&self) -> Vec<String> {
        let mut x: Vec<_> = self
            .index
            .classes
            .iter()
            .map(|c| {
                if c.package.is_empty() {
                    "(root)".into()
                } else {
                    c.package.clone()
                }
            })
            .collect();
        x.sort();
        x.dedup();
        x
    }
    fn classes(&self) -> Vec<&Class> {
        let p = self
            .packages()
            .get(self.package)
            .cloned()
            .unwrap_or_default();
        self.index
            .classes
            .iter()
            .filter(|c| {
                if p == "(root)" {
                    c.package.is_empty()
                } else {
                    c.package == p
                }
            })
            .collect()
    }
    fn current(&self) -> Option<&Class> {
        self.classes().get(self.class).copied()
    }
    fn protocols(&self) -> Vec<String> {
        let Some(c) = self.current() else {
            return vec![];
        };
        let mut x: Vec<_> = c.methods.iter().map(|m| m.side.clone()).collect();
        x.sort();
        x.dedup();
        x
    }
    fn methods(&self) -> Vec<&Method> {
        let Some(c) = self.current() else {
            return vec![];
        };
        let p = self
            .protocols()
            .get(self.protocol)
            .cloned()
            .unwrap_or_default();
        c.methods.iter().filter(|m| m.side == p).collect()
    }
    fn selected(&self) -> Option<(&Class, &Method)> {
        Some((self.current()?, self.methods().get(self.method).copied()?))
    }
    fn source(&self) -> String {
        let Some((c, m)) = self.selected() else {
            return "Select a method".into();
        };
        let root = std::fs::canonicalize(&self.index.root).ok();
        let path = std::fs::canonicalize(&c.path).ok();
        if !matches!((&root,&path),(Some(r),Some(p)) if p.starts_with(r)) {
            return "Source is outside this browser root".into();
        }
        let Ok(text) = std::fs::read_to_string(path.unwrap()) else {
            return "Source unavailable".into();
        };
        let lines: Vec<_> = text.lines().collect();
        let start = m.line.saturating_sub(1);
        let end = c
            .methods
            .iter()
            .filter(|x| x.line > m.line)
            .map(|x| x.line.saturating_sub(1))
            .min()
            .unwrap_or(lines.len());
        lines.get(start..end).unwrap_or(&[]).join("\n")
    }
    fn move_by(&mut self, delta: isize) {
        let n = match self.pane {
            Pane::Packages => self.packages().len(),
            Pane::Classes => self.classes().len(),
            Pane::Protocols => self.protocols().len(),
            Pane::Methods => self.methods().len(),
        };
        if n == 0 {
            return;
        };
        let v = match self.pane {
            Pane::Packages => &mut self.package,
            Pane::Classes => &mut self.class,
            Pane::Protocols => &mut self.protocol,
            Pane::Methods => &mut self.method,
        };
        *v = ((*v as isize + delta).rem_euclid(n as isize)) as usize;
        if !matches!(self.pane, Pane::Methods) {
            self.method = 0
        };
        self.source_scroll = 0
    }
    fn key(&mut self, k: KeyCode) -> bool {
        match k {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Left => {
                self.pane = match self.pane {
                    Pane::Packages => Pane::Packages,
                    Pane::Classes => Pane::Packages,
                    Pane::Protocols => Pane::Classes,
                    Pane::Methods => Pane::Protocols,
                }
            }
            KeyCode::Right | KeyCode::Enter => {
                self.pane = match self.pane {
                    Pane::Packages => Pane::Classes,
                    Pane::Classes => Pane::Protocols,
                    Pane::Protocols => Pane::Methods,
                    Pane::Methods => Pane::Methods,
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.source_scroll = self.source_scroll.saturating_sub(12),
            KeyCode::PageDown => self.source_scroll += 12,
            _ => {}
        };
        false
    }
}
fn list<'a>(
    title: &'a str,
    rows: Vec<String>,
    selected: usize,
    focused: bool,
    height: u16,
) -> List<'a> {
    let visible = usize::from(height.saturating_sub(2)).max(1);
    let start = selected.saturating_sub(visible.saturating_sub(1));
    List::new(
        rows.into_iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, row)| {
                ListItem::new(if i == selected {
                    format!("> {row}")
                } else {
                    row
                })
            })
            .collect::<Vec<_>>(),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(if focused {
                Color::Cyan
            } else {
                Color::DarkGray
            })),
    )
}
fn draw(f: &mut Frame, a: &App) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(38),
        Constraint::Percentage(62),
    ])
    .split(f.area());
    let cols = Layout::horizontal([
        Constraint::Percentage(20),
        Constraint::Percentage(27),
        Constraint::Percentage(20),
        Constraint::Percentage(33),
    ])
    .split(areas[1]);
    let p = a.packages();
    let cs = a.classes();
    let ps = a.protocols();
    let ms = a.methods();
    f.render_widget(Paragraph::new("Trashtalk Browser · ↑/↓ select · ←/→ pane · Enter drill in · PgUp/PgDn source · q close").block(Block::default().borders(Borders::ALL).title(" Read-only ")),areas[0]);
    f.render_widget(
        list(
            "Packages",
            p,
            a.package,
            matches!(a.pane, Pane::Packages),
            cols[0].height,
        ),
        cols[0],
    );
    f.render_widget(
        list(
            "Classes",
            cs.iter()
                .map(|x| {
                    format!(
                        "{} {}",
                        if x.superclass.is_empty() { "" } else { "↳" },
                        x.name
                    )
                })
                .collect(),
            a.class,
            matches!(a.pane, Pane::Classes),
            cols[1].height,
        ),
        cols[1],
    );
    f.render_widget(
        list(
            "Protocol",
            ps,
            a.protocol,
            matches!(a.pane, Pane::Protocols),
            cols[2].height,
        ),
        cols[2],
    );
    f.render_widget(
        list(
            "Methods",
            ms.iter()
                .map(|x| format!("{}{}", if x.raw { "raw " } else { "" }, x.selector))
                .collect(),
            a.method,
            matches!(a.pane, Pane::Methods),
            cols[3].height,
        ),
        cols[3],
    );
    let source = a.source();
    let lines: Vec<Line> = source
        .lines()
        .skip(a.source_scroll)
        .enumerate()
        .map(|(i, x)| Line::raw(format!("{:4}  {x}", i + a.source_scroll + 1)))
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Source ")),
        areas[2],
    );
}
pub fn run(input: &str, height: u16) -> Result<()> {
    let mut a = App::new(parse(input)?);
    let stop = termination_flag()?;
    let mut term = InlineTerminal::enter(height.max(12))?;
    loop {
        term.draw(|f| draw(f, &a))?;
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        };
        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) && a.key(k.code) {
                    break;
                }
            }
        }
    }
    Ok(())
}
