//! Retained presentation toolkit. Bash supplies batches; all local interaction
//! and painting run here with no model callbacks.
pub mod data;
pub mod model;
pub mod profile;
pub mod protocol;
pub mod render;
pub mod transport;
use crate::inline_terminal::{InlineTerminal, termination_flag};
use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use model::App;
use render::Renderer;
use std::{
    path::Path,
    sync::{atomic::Ordering, mpsc::TryRecvError},
    time::{Duration, Instant},
};
use transport::Transport;

pub fn run(height: u16, profile_path: Option<&Path>) -> Result<()> {
    let transport = Transport::new(std::io::stdin(), std::io::stdout());
    let mut app = App::new(profile_path.is_some());
    let mut renderer = Renderer::default();
    let stop = termination_flag()?;
    let mut terminal = InlineTerminal::enter(height)?;
    terminal.enable_mouse_capture()?;
    terminal.enable_bracketed_paste()?;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut local_input = None;
    let mut dragging: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        // Budget the drain so a fast producer cannot starve terminal input.
        if app.connected {
            let drain_started = Instant::now();
            for _ in 0..protocol::QUEUE_FRAMES {
                match transport.input.try_recv() {
                    Ok(Ok(bytes)) => {
                        let started = app.profile.start();
                        let decoded = serde_json::from_slice::<protocol::Frame>(&bytes)
                            .map_err(anyhow::Error::from);
                        app.profile.end("protocol_decode", started);
                        match decoded.and_then(|frame| app.apply(frame)) {
                            Ok(()) => {}
                            Err(error) => {
                                app.status = format!("Rejected update: {error}");
                                app.profile.count("rejected_frames", 1);
                                app.dirty = true;
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        app.disconnected();
                        app.status = format!("Disconnected: {error}");
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        app.disconnected();
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
                if drain_started.elapsed() >= Duration::from_millis(2) {
                    break;
                }
            }
        }
        if transport.failed.try_recv().is_ok() {
            app.disconnected();
        }
        let mut input_event = false;
        if event::poll(Duration::from_millis(4))? {
            let started = app.profile.start();
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if key.code == KeyCode::F(12) {
                        if let Some(path) = profile_path {
                            write_profile(&mut app, &transport, path)?;
                            app.status = "Profile summary written".into();
                            app.dirty = true;
                        }
                        continue;
                    }
                    if key.code == KeyCode::F(11) {
                        if profile_path.is_some() {
                            app.profile = profile::Profile::new(true);
                            app.profile.view = app.view.clone();
                            transport.traffic.received.store(0, Ordering::Relaxed);
                            transport.traffic.sent.store(0, Ordering::Relaxed);
                            transport.traffic.peak.store(
                                transport.traffic.queued.load(Ordering::Relaxed),
                                Ordering::Relaxed,
                            );
                            app.status = "Native profiling counters reset".into();
                            app.dirty = true;
                        }
                        continue;
                    }
                    if !terminal.handle_resize_key(key, 5)? && !app.key(key) {
                        break;
                    }
                    app.dirty = true;
                    input_event = true;
                }
                Event::Paste(text) => {
                    app.paste(&text);
                    input_event = true;
                }
                Event::Resize(..) => {
                    app.dirty = true;
                    input_event = true;
                }
                Event::Mouse(mouse) => {
                    if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        dragging = renderer
                            .dividers
                            .iter()
                            .find(|(_, r)| r.contains((mouse.column, mouse.row).into()))
                            .map(|(key, _)| key.clone());
                    }
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        dragging = None;
                    }
                    if let Some(key) = &dragging
                        && matches!(mouse.kind, MouseEventKind::Drag(_))
                        && let Some(area) = renderer.areas.get(key)
                    {
                        let horizontal = app.nodes[key].1.direction == "row";
                        let (pos, len) = if horizontal {
                            (mouse.column.saturating_sub(area.x), area.width)
                        } else {
                            (mouse.row.saturating_sub(area.y), area.height)
                        };
                        app.local.get_mut(key).unwrap().split =
                            ((u32::from(pos) * 100 / u32::from(len.max(1))) as u16).clamp(10, 90);
                        app.dirty = true;
                        input_event = true;
                    }
                    let target =
                        app.focus_order
                            .iter()
                            .rev()
                            .find(|key| {
                                renderer.areas.get(*key).is_some_and(|area| {
                                    area.contains((mouse.column, mouse.row).into())
                                })
                            })
                            .cloned();
                    if let Some(key) = target {
                        if !matches!(mouse.kind, MouseEventKind::Moved) {
                            app.focus = key.clone();
                        }
                        if app.nodes[&key].0 == protocol::Kind::Plot {
                            let area = renderer.areas[&key];
                            app.local.get_mut(&key).unwrap().plot_cursor = Some(
                                mouse
                                    .column
                                    .saturating_sub(area.x + u16::from(app.nodes[&key].1.border))
                                    as usize,
                            );
                        }
                        match mouse.kind {
                            MouseEventKind::ScrollDown => {
                                app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                            }
                            MouseEventKind::ScrollUp => {
                                app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                            }
                            MouseEventKind::Down(_) => {
                                if app.nodes.get(&key).is_some_and(|(k, _)| {
                                    matches!(k, protocol::Kind::Button | protocol::Kind::Toggle)
                                }) {
                                    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                                } else if app.nodes.get(&key).is_some_and(|(k, _)| {
                                    matches!(k, protocol::Kind::List | protocol::Kind::Table)
                                }) {
                                    let area = renderer.areas[&key];
                                    let border = u16::from(app.nodes[&key].1.border);
                                    let state = app.local.get_mut(&key).unwrap();
                                    state.selected = state.offset
                                        + mouse.row.saturating_sub(area.y + border) as usize;
                                    state.following = false;
                                    if let Some(data) = app.data.get(&app.nodes[&key].1.source) {
                                        state.selected =
                                            state.selected.min(data.total.saturating_sub(1));
                                        state.selected_key =
                                            data.row(state.selected).map(|r| r.key.clone());
                                    }
                                }
                            }
                            _ => {}
                        }
                        app.dirty = true;
                        input_event = true;
                    }
                }
                _ => {}
            }
            if input_event && local_input.is_none() {
                local_input = started;
            }
        }
        app.windows();
        app.queries();
        if let Some(intent) = app.take_intent() {
            let mut bytes = serde_json::to_vec(&intent)?;
            bytes.push(b'\n');
            match transport.send(bytes) {
                Ok(true) => {}
                Ok(false) => app.return_intent(intent),
                Err(_) => app.disconnected(),
            }
        }
        if app.dirty && (input_event || last_draw.elapsed() >= Duration::from_micros(16_667)) {
            let started = app.profile.start();
            terminal.draw(|frame| {
                let start = app.profile.start();
                renderer.draw(&mut app, frame);
                app.profile.end("buffer_render_including_layout", start);
            })?;
            app.profile.end("draw_and_terminal_write", started);
            app.profile
                .end("local_input_to_terminal_write", local_input.take());
            app.profile.rendered();
            app.profile.count("redraws", 1);
            app.dirty = false;
            last_draw = Instant::now();
        }
    }
    drop(terminal);
    if let Some(path) = profile_path {
        write_profile(&mut app, &transport, path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

pub mod plot;

fn write_profile(app: &mut App, transport: &Transport, path: &Path) -> Result<()> {
    app.metrics();
    app.profile.gauge(
        "bytes_in",
        transport.traffic.received.load(Ordering::Relaxed),
    );
    app.profile
        .gauge("bytes_out", transport.traffic.sent.load(Ordering::Relaxed));
    app.profile.gauge(
        "queued_bytes_peak",
        transport.traffic.peak.load(Ordering::Relaxed),
    );
    std::fs::write(path, serde_json::to_vec_pretty(&app.profile)?)?;

    Ok(())
}
