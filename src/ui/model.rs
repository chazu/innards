use super::{data::Data, profile::Profile, protocol::*};
use anyhow::{Result, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Instant,
};

#[derive(Default)]
pub struct Local {
    pub generation: u64,
    pub query_due: Option<Instant>,
    pub draft: String,
    pub split: u16,
    pub plot_domain: Option<(f64, f64)>,
    pub plot_cursor: Option<usize>,
    pub cursor: usize,
    pub edited: bool,
    pub selected: usize,
    pub selected_key: Option<String>,
    pub offset: usize,
    pub following: bool,
    pub unread: usize,
}
struct Pending {
    intent: IntentKind,
    started: Option<Instant>,
    draft: Option<(String, String)>,
}
pub struct App {
    pub view: String,
    pub revision: u64,
    pub root: Option<Node>,
    pub nodes: HashMap<String, (Kind, Props)>,
    pub local: HashMap<String, Local>,
    pub data: HashMap<String, Data>,
    pub focus_order: Vec<String>,
    pub focus: String,
    pub connected: bool,
    pub status: String,
    pub profile: Profile,
    pub dirty: bool,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    outbox: VecDeque<Intent>,
    pub wanted: HashMap<String, usize>,
    pub visible: HashMap<String, usize>,
}
type PreparedTree = (Node, HashMap<String, (Kind, Props)>, Vec<String>);
fn prepare_tree(mut root: Node) -> Result<PreparedTree> {
    fn walk(
        node: &mut Node,
        path: String,
        nodes: &mut HashMap<String, (Kind, Props)>,
        focus: &mut Vec<String>,
        depth: usize,
        hidden: bool,
    ) -> Result<()> {
        ensure!(
            depth < 32 && nodes.len() < MAX_NODES,
            "widget tree limit exceeded"
        );
        if node.key.is_empty() {
            node.key = path;
        }
        ensure!(!nodes.contains_key(&node.key), "duplicate widget key");
        ensure!(
            matches!(node.kind, Kind::Panel | Kind::Split) || node.children.is_empty(),
            "only panels and splits have children"
        );
        ensure!(
            node.props.direction.is_empty()
                || ["row", "column"].contains(&node.props.direction.as_str()),
            "invalid direction"
        );
        ensure!(
            node.props.padding <= 16 && node.props.gap <= 16,
            "layout bounds exceeded"
        );
        ensure!(
            node.kind != Kind::Split || node.children.len() == 2,
            "split needs two children"
        );
        ensure!(
            node.props
                .debounce_ms
                .is_none_or(|ms| (50..=10000).contains(&ms)),
            "debounce must be 50..10000 ms"
        );
        ensure!(
            node.props.options.len() <= 128
                && node.props.columns.len() <= 32
                && node.props.inputs.len() <= 64
                && node.props.cells.len() <= 4096,
            "widget data limit exceeded"
        );
        ensure!(
            node.props
                .cells
                .iter()
                .all(|cell| !cell.text.chars().any(char::is_control)),
            "control character in canvas glyph"
        );
        ensure!(
            node.props
                .domain_y
                .is_none_or(|[lo, hi]| lo.is_finite() && hi.is_finite() && lo < hi),
            "invalid plot Y domain"
        );
        ensure!(
            serde_json::to_vec(&node.props)?.len() <= 65536,
            "widget property byte limit"
        );
        let hidden = hidden || node.props.hidden;
        if !hidden
            && !node.props.disabled
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
            focus.push(node.key.clone());
        }
        nodes.insert(node.key.clone(), (node.kind.clone(), node.props.clone()));
        for (i, child) in node.children.iter_mut().enumerate() {
            walk(
                child,
                format!("{}/{}", node.key, i),
                nodes,
                focus,
                depth + 1,
                hidden,
            )?;
        }
        Ok(())
    }
    let mut nodes = HashMap::new();
    let mut focus = Vec::new();
    walk(&mut root, "@root".into(), &mut nodes, &mut focus, 0, false)?;
    for (_, p) in nodes.values() {
        for name in [&p.input, &p.nonempty] {
            ensure!(
                name.is_empty() || nodes.get(name).is_some_and(|(k, _)| *k == Kind::Input),
                "input reference missing"
            );
        }
        ensure!(
            p.inputs.iter().all(|name| nodes
                .get(name)
                .is_some_and(|(k, _)| matches!(k, Kind::Input | Kind::Toggle | Kind::Select))),
            "form input reference missing"
        );
        ensure!(
            p.detail_of.is_empty()
                || nodes
                    .get(&p.detail_of)
                    .is_some_and(|(k, _)| matches!(k, Kind::List | Kind::Table)),
            "detail list missing"
        );
    }
    Ok((root, nodes, focus))
}
impl App {
    pub fn new(profile: bool) -> Self {
        Self {
            view: String::new(),
            revision: 0,
            root: None,
            nodes: HashMap::new(),
            local: HashMap::new(),
            data: HashMap::new(),
            focus_order: Vec::new(),
            focus: String::new(),
            connected: true,
            status: "Waiting for a view…".into(),
            profile: Profile::new(profile),
            dirty: true,
            next_id: 1,
            pending: HashMap::new(),
            outbox: VecDeque::new(),
            wanted: HashMap::new(),
            visible: HashMap::new(),
        }
    }
    fn tree(&mut self, root: Node) -> Result<()> {
        let (root, nodes, focus) = prepare_tree(root)?;
        self.local.retain(|key, _| {
            nodes
                .get(key)
                .zip(self.nodes.get(key))
                .is_some_and(|(a, b)| a.0 == b.0)
        });
        for (key, (kind, p)) in &nodes {
            let state = self.local.entry(key.clone()).or_insert_with(|| Local {
                split: 50,
                draft: p.value.clone(),
                cursor: p.value.len(),
                following: p.follow_end,
                ..Local::default()
            });
            if matches!(kind, Kind::Input | Kind::Toggle | Kind::Select) && !state.edited {
                state.draft = p.value.clone();
                state.cursor = state.cursor.min(state.draft.len());
                while !state.draft.is_char_boundary(state.cursor) {
                    state.cursor -= 1;
                }
            }
        }
        let requested = focus
            .iter()
            .find(|key| {
                nodes[*key].1.focused && self.nodes.get(*key).is_none_or(|(_, p)| !p.focused)
            })
            .cloned();
        self.nodes = nodes;
        self.root = Some(root);
        self.focus_order = focus;
        if let Some(key) = requested {
            self.focus = key;
        }
        if !self.focus_order.contains(&self.focus) {
            self.focus = self.focus_order.first().cloned().unwrap_or_default();
        }
        Ok(())
    }
    pub fn apply(&mut self, frame: Frame) -> Result<()> {
        let started = self.profile.start();
        ensure!(
            frame.schema_version == 1 && !frame.view.is_empty() && frame.view.len() <= 128,
            "unsupported UI envelope"
        );
        ensure!(
            !self.view.is_empty() || matches!(frame.message, Message::Init { .. }),
            "initialize view before sending updates"
        );
        ensure!(
            self.view.is_empty() || self.view == frame.view,
            "view identity mismatch"
        );
        self.profile.count("frames_in", 1);
        let cause = frame.caused_by.or(match &frame.message {
            Message::Window { request_id, .. } | Message::QueryResult { request_id, .. } => {
                Some(*request_id)
            }
            _ => None,
        });
        match frame.message {
            Message::Init {
                revision,
                root,
                collections,
            } => {
                ensure!(collections.len() <= 16, "too many collections");
                let mut data = HashMap::new();
                for collection in collections {
                    let key = collection.id.clone();
                    ensure!(!data.contains_key(&key), "duplicate collection");
                    data.insert(key, Data::new(collection)?);
                }
                ensure!(
                    data.values().map(Data::cached).sum::<usize>() <= MAX_ROWS,
                    "initial cache limit"
                );
                self.tree(root)?;
                self.data = data;
                self.view = frame.view;
                self.revision = revision;
                if self.profile.enabled {
                    self.profile.view = self.view.clone();
                }
                // Read requests may be retried after resync; actions are never replayed.
                self.pending
                    .retain(|_, p| matches!(p.intent, IntentKind::Action { .. }));
                self.outbox.clear();
                self.wanted.clear();
                self.status = String::new();
            }
            Message::Tree {
                base_revision,
                revision,
                root,
            } => {
                if base_revision != self.revision || revision <= base_revision {
                    self.resync("view".into(), self.revision);
                    return Ok(());
                }
                self.tree(root)?;
                self.revision = revision;
            }
            Message::Properties {
                base_revision,
                revision,
                updates,
            } => {
                if base_revision != self.revision || revision <= base_revision {
                    self.resync("view".into(), self.revision);
                    return Ok(());
                }
                ensure!(updates.len() <= MAX_NODES, "property batch too large");
                let mut props = HashMap::new();
                for update in updates {
                    ensure!(
                        self.nodes.contains_key(&update.key) && !props.contains_key(&update.key),
                        "unknown or duplicate property target"
                    );
                    props.insert(update.key, update.props);
                }
                fn patch(node: &mut Node, props: &mut HashMap<String, Props>) {
                    if let Some(p) = props.remove(&node.key) {
                        node.props = p;
                    }
                    for child in &mut node.children {
                        patch(child, props);
                    }
                }
                let mut root = self
                    .root
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("view not initialized"))?;
                patch(&mut root, &mut props);
                self.tree(root)?;
                self.revision = revision;
            }
            Message::Collection { collection } => {
                let source = collection.id.clone();
                let data = Data::new(collection)?;
                ensure!(
                    self.data.contains_key(&source) || self.data.len() < 16,
                    "collection limit"
                );
                ensure!(
                    self.data
                        .get(&source)
                        .is_none_or(|d| data.revision >= d.revision),
                    "old collection snapshot"
                );
                self.data.insert(source.clone(), data);
                self.finish_reads(&source);
                self.wanted.remove(&source);
            }
            Message::Change {
                source,
                base_revision,
                revision,
                changes,
            } => {
                let Some(data) = self.data.get_mut(&source) else {
                    self.resync(source, 0);
                    return Ok(());
                };
                let previous = data.total;
                let dropped = data.dropped;
                if data.change(base_revision, revision, changes).is_err() {
                    let rev = data.revision;
                    self.resync(source, rev);
                    return Ok(());
                }
                let removed = data.dropped - dropped;
                let added = (data.total + removed).saturating_sub(previous);
                for (key, (_, p)) in &self.nodes {
                    if p.source == source {
                        let state = self.local.get_mut(key).unwrap();
                        state.offset = state.offset.saturating_sub(removed);
                        state.selected = state
                            .selected_key
                            .as_deref()
                            .and_then(|key| data.index_of(key))
                            .unwrap_or_else(|| state.selected.saturating_sub(removed))
                            .min(data.total.saturating_sub(1));
                        if !state.following {
                            state.unread += added;
                        }
                    }
                }
                self.profile.count("retention_dropped", removed);
            }
            Message::Window {
                source,
                revision,
                request_id,
                start,
                total,
                rows,
            } => {
                let Some(pending) = self.pending.remove(&request_id) else {
                    self.profile.count("stale_replies", 1);
                    return Ok(());
                };
                match pending.intent {
                    IntentKind::Window {
                        source: ref expected,
                        revision: rev,
                        start: asked,
                        count,
                    } if *expected == source
                        && rev == revision
                        && asked == start
                        && rows.len() == count.min(total.saturating_sub(start)) => {}
                    _ => {
                        self.pending.insert(request_id, pending);
                        anyhow::bail!("window response does not match request");
                    }
                }
                self.profile.end("window_roundtrip", pending.started);
                if self.wanted.get(&source) != Some(&start)
                    || self
                        .data
                        .get(&source)
                        .is_none_or(|d| d.revision != revision)
                {
                    self.profile.count("stale_replies", 1);
                    return Ok(());
                }
                if let Some(data) = self.data.get_mut(&source) {
                    data.window(start, total, rows, start)?;
                }
            }
            Message::QueryResult {
                request_id,
                generation,
                collection,
                message,
            } => {
                let Some(p) = self.pending.remove(&request_id) else {
                    self.profile.count("stale_replies", 1);
                    return Ok(());
                };
                let IntentKind::Query {
                    widget,
                    generation: asked,
                    ..
                } = &p.intent
                else {
                    self.pending.insert(request_id, p);
                    anyhow::bail!("query result does not match request");
                };
                if *asked != generation
                    || self
                        .local
                        .get(widget)
                        .is_none_or(|s| s.generation != generation)
                {
                    self.profile.count("stale_replies", 1);
                    return Ok(());
                }
                self.profile.end("query_roundtrip", p.started);
                if let Some(collection) = collection {
                    let source = collection.id.clone();
                    let data = Data::new(collection)?;
                    if self
                        .data
                        .get(&source)
                        .is_some_and(|old| old.revision > data.revision)
                    {
                        self.profile.count("stale_replies", 1);
                        return Ok(());
                    }
                    ensure!(
                        self.data.contains_key(&source) || self.data.len() < 16,
                        "collection limit"
                    );
                    self.data.insert(source.clone(), data);
                    self.finish_reads(&source);
                    self.wanted.remove(&source);
                }
                self.status = message;
            }
            Message::Ack {
                request_id,
                ok,
                message,
            } => {
                if let Some(p) = self.pending.remove(&request_id) {
                    self.profile.ack(request_id);
                    self.profile.end("ack_roundtrip", p.started);
                    if ok
                        && let Some((key, sent)) = p.draft
                        && let Some(state) = self.local.get_mut(&key)
                        && state.draft == sent
                    {
                        state.generation += 1;
                        state.query_due = None;
                        state.draft.clear();
                        state.cursor = 0;
                        state.edited = false;
                    }
                    self.status = message;
                } else {
                    self.profile.count("stale_replies", 1);
                }
            }
        }
        if let Some(id) = cause {
            self.profile.updated(id);
        }
        self.dirty = true;
        self.profile.end("reconcile", started);
        Ok(())
    }
    fn finish_reads(&mut self, source: &str) {
        let ids: HashSet<u64> = self
            .pending
            .iter()
            .filter_map(|(id, p)| match &p.intent {
                IntentKind::Window { source: s, .. } | IntentKind::Resync { target: s, .. }
                    if s == source =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        self.pending.retain(|id, _| !ids.contains(id));
        self.outbox.retain(|p| !ids.contains(&p.request_id));
    }
    fn enqueue(&mut self, intent: IntentKind, draft: Option<(String, String)>) -> bool {
        if !self.connected || self.view.is_empty() || self.pending.len() >= MAX_IN_FLIGHT {
            self.status = "Waiting for application; local navigation remains available".into();
            return false;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.profile.request(id);
        self.pending.insert(
            id,
            Pending {
                intent: intent.clone(),
                started: self.profile.start(),
                draft,
            },
        );
        self.outbox.push_back(Intent {
            schema_version: 1,
            view: self.view.clone(),
            request_id: id,
            intent,
        });
        self.profile.count("requests_out", 1);
        self.profile.peak("in_flight_peak", self.pending.len());
        true
    }
    fn resync(&mut self, target: String, revision: u64) {
        if self
            .pending
            .values()
            .any(|p| matches!(&p.intent,IntentKind::Resync{target:t,..} if t==&target))
        {
            return;
        }
        self.status = "Revision changed; requesting a fresh snapshot".into();
        self.dirty = true;
        if self.enqueue(IntentKind::Resync { target, revision }, None) {
            self.profile.count("resyncs", 1);
        }
    }
    pub fn take_intent(&mut self) -> Option<Intent> {
        self.outbox.pop_front()
    }
    pub fn return_intent(&mut self, intent: Intent) {
        self.outbox.push_front(intent);
    }
    pub fn disconnected(&mut self) {
        self.connected = false;
        self.outbox.clear();
        self.status = if self
            .pending
            .values()
            .any(|p| matches!(p.intent, IntentKind::Action { .. }))
        {
            "Disconnected; unacknowledged action outcome unknown. Draft retained."
        } else {
            "Disconnected; cached view remains available"
        }
        .into();
        self.dirty = true;
    }
    pub fn queries(&mut self) {
        let now = Instant::now();
        let ready: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(key, (kind, p))| {
                let state = self.local.get(key)?;
                (*kind == Kind::Input
                    && p.debounce_ms.is_some()
                    && !p.action.is_empty()
                    && state.query_due.is_some_and(|due| due <= now))
                .then(|| {
                    (
                        key.clone(),
                        p.action.clone(),
                        state.draft.clone(),
                        state.generation,
                    )
                })
            })
            .collect();
        for (widget, action, value, generation) in ready {
            if self
                .pending
                .values()
                .any(|p| matches!(&p.intent,IntentKind::Query{widget:key,..} if key==&widget))
            {
                continue;
            }
            if self.enqueue(
                IntentKind::Query {
                    widget: widget.clone(),
                    action,
                    value,
                    generation,
                },
                None,
            ) {
                self.local.get_mut(&widget).unwrap().query_due = None;
            }
        }
    }
    pub fn windows(&mut self) {
        let mut requests = Vec::new();
        for (key, (kind, p)) in &self.nodes {
            if !matches!(kind, Kind::List | Kind::Table) || p.hidden {
                continue;
            }
            let Some(data) = self.data.get(&p.source) else {
                continue;
            };
            if !data.windowed {
                continue;
            }
            let offset = self.local[key].offset;
            let visible = *self.visible.get(key).unwrap_or(&20);
            // Fetch one neighboring block ahead, retaining already cached ranges.
            let end = (offset + visible + 32).min(data.total);
            if let Some(missing) = (offset..end).find(|i| data.row(*i).is_none()) {
                let start = (missing / WINDOW_ROWS) * WINDOW_ROWS;
                requests.push((p.source.clone(), data.revision, start));
            }
        }
        for (source, revision, start) in requests {
            if self
                .wanted
                .insert(source.clone(), start)
                .is_some_and(|old| old != start)
            {
                self.profile.count("coalesced_viewports", 1);
            }
            if !self
                .pending
                .values()
                .any(|p| matches!(&p.intent,IntentKind::Window{source:s,..} if *s==source))
            {
                self.profile.count("cache_misses", 1);
                self.enqueue(
                    IntentKind::Window {
                        source,
                        revision,
                        start,
                        count: WINDOW_ROWS,
                    },
                    None,
                );
            }
        }
    }
    pub fn enabled(&self, key: &str) -> bool {
        self.nodes.get(key).is_some_and(|(_, p)| {
            !p.disabled
                && (p.nonempty.is_empty()
                    || self
                        .local
                        .get(&p.nonempty)
                        .is_some_and(|s| !s.draft.trim().is_empty()))
        })
    }
    pub fn activate(&mut self, key: &str) {
        if !self.enabled(key) {
            return;
        }
        let Some((kind, p)) = self.nodes.get(key).cloned() else {
            return;
        };
        if p.action.is_empty() {
            return;
        }
        if self
            .pending
            .values()
            .any(|v| matches!(&v.intent,IntentKind::Action{widget,..} if widget==key))
        {
            self.status = "Action is awaiting acknowledgement".into();
            return;
        }
        let input = if kind == Kind::Input { key } else { &p.input };
        let draft = self.local.get(input).map(|s| s.draft.clone());
        let value = if !p.inputs.is_empty() {
            let values: serde_json::Map<_, _> = p
                .inputs
                .iter()
                .filter_map(|key| {
                    self.local.get(key).map(|state| {
                        (
                            key.clone(),
                            if self.nodes[key].0 == Kind::Toggle {
                                json!(state.draft == "true")
                            } else {
                                json!(state.draft)
                            },
                        )
                    })
                })
                .collect();
            json!(values)
        } else if kind == Kind::Toggle {
            json!(self.local[key].draft == "true")
        } else if kind == Kind::Select {
            json!(self.local[key].draft)
        } else if let Some(text) = &draft {
            json!(text)
        } else if matches!(kind, Kind::List | Kind::Table) {
            self.selected_row(key)
                .map(|r| json!({"key":r.key,"fields":r.fields}))
                .unwrap_or_default()
        } else {
            json!(null)
        };
        self.enqueue(
            IntentKind::Action {
                widget: key.into(),
                action: p.action,
                value,
            },
            if p.clear_on_ack {
                draft.map(|s| (input.into(), s))
            } else {
                None
            },
        );
    }
    pub fn selected_row(&self, key: &str) -> Option<&Row> {
        let (_, p) = self.nodes.get(key)?;
        self.data.get(&p.source)?.row(self.local.get(key)?.selected)
    }
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            if let Some(root) = &self.root {
                let action = if key.code == KeyCode::Left {
                    root.props.back_action.clone()
                } else {
                    root.props.forward_action.clone()
                };
                if !action.is_empty() {
                    let widget = root.key.clone();
                    self.enqueue(
                        IntentKind::Action {
                            widget,
                            action,
                            value: json!(null),
                        },
                        None,
                    );
                }
            }
            self.dirty = true;
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && !self.focus_order.is_empty() {
            let i = self
                .focus_order
                .iter()
                .position(|k| *k == self.focus)
                .unwrap_or(0);
            let n = self.focus_order.len();
            self.focus = self.focus_order[(i + if key.code == KeyCode::BackTab {
                n - 1
            } else {
                1
            }) % n]
                .clone();
            self.dirty = true;
            return true;
        }
        let focus = self.focus.clone();
        let Some((kind, p)) = self.nodes.get(&focus).cloned() else {
            return true;
        };
        if !self.enabled(&focus) {
            return true;
        }
        match kind {
            Kind::Input => {
                if key.code == KeyCode::Enter
                    && (!p.multiline || key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.activate(&focus);
                } else {
                    let state = self.local.get_mut(&focus).unwrap();
                    let before_len = state.draft.len();
                    match key.code {
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            if state.draft.len() + c.len_utf8() <= 65536 {
                                state.draft.insert(state.cursor, c);
                                state.cursor += c.len_utf8();
                                state.edited = true;
                            }
                        }
                        KeyCode::Enter if p.multiline => {
                            if state.draft.len() < 65536 {
                                state.draft.insert(state.cursor, '\n');
                                state.cursor += 1;
                                state.edited = true;
                            }
                        }
                        KeyCode::Backspace if state.cursor > 0 => {
                            let prev = state.draft[..state.cursor].char_indices().last().unwrap().0;
                            state.draft.drain(prev..state.cursor);
                            state.cursor = prev;
                            state.edited = true;
                        }
                        KeyCode::Delete if state.cursor < state.draft.len() => {
                            let len = state.draft[state.cursor..]
                                .chars()
                                .next()
                                .unwrap()
                                .len_utf8();
                            state.draft.drain(state.cursor..state.cursor + len);
                            state.edited = true;
                        }
                        KeyCode::Left if state.cursor > 0 => {
                            state.cursor =
                                state.draft[..state.cursor].char_indices().last().unwrap().0;
                        }
                        KeyCode::Right if state.cursor < state.draft.len() => {
                            state.cursor += state.draft[state.cursor..]
                                .chars()
                                .next()
                                .unwrap()
                                .len_utf8();
                        }
                        KeyCode::Home => state.cursor = 0,
                        KeyCode::End => state.cursor = state.draft.len(),
                        _ => {}
                    }
                    if state.draft.len() != before_len {
                        state.generation += 1;
                        if let Some(ms) = p.debounce_ms {
                            state.query_due =
                                Some(Instant::now() + std::time::Duration::from_millis(ms));
                        }
                    }
                }
            }
            Kind::List | Kind::Table => {
                if key.code == KeyCode::Enter {
                    self.activate(&focus);
                } else {
                    let total = self.data.get(&p.source).map_or(0, |d| d.total);
                    let height = *self.visible.get(&focus).unwrap_or(&10);
                    let state = self.local.get_mut(&focus).unwrap();
                    let target = match key.code {
                        KeyCode::Down => state.selected.saturating_add(1),
                        KeyCode::Up => state.selected.saturating_sub(1),
                        KeyCode::PageDown => state.selected.saturating_add(height),
                        KeyCode::PageUp => state.selected.saturating_sub(height),
                        KeyCode::Home => 0,
                        KeyCode::End => total.saturating_sub(1),
                        _ => state.selected,
                    }
                    .min(total.saturating_sub(1));
                    state.selected = target;
                    state.selected_key = self
                        .data
                        .get(&p.source)
                        .and_then(|d| d.row(target))
                        .map(|r| r.key.clone());
                    state.following = p.follow_end && target == total.saturating_sub(1);
                    if state.following {
                        state.unread = 0;
                    }
                    if target < state.offset {
                        state.offset = target;
                    }
                    if target >= state.offset + height {
                        state.offset = target.saturating_sub(height.saturating_sub(1));
                    }
                }
            }
            Kind::Toggle if key.code == KeyCode::Enter || key.code == KeyCode::Char(' ') => {
                let state = self.local.get_mut(&focus).unwrap();
                state.draft = if state.draft == "true" {
                    "false"
                } else {
                    "true"
                }
                .into();
                state.edited = true;
                self.activate(&focus);
            }
            Kind::Select => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                ) && !p.options.is_empty()
                {
                    let state = self.local.get_mut(&focus).unwrap();
                    let i = p
                        .options
                        .iter()
                        .position(|s| *s == state.draft)
                        .unwrap_or(0);
                    let n = p.options.len();
                    let step = if matches!(key.code, KeyCode::Left | KeyCode::Up) {
                        n - 1
                    } else {
                        1
                    };
                    state.draft = p.options[(i + step) % n].clone();
                    state.edited = true;
                }
                if key.code == KeyCode::Enter {
                    self.activate(&focus);
                }
            }
            Kind::Split => {
                let state = self.local.get_mut(&focus).unwrap();
                match key.code {
                    KeyCode::Left | KeyCode::Up => {
                        state.split = state.split.saturating_sub(5).max(10)
                    }
                    KeyCode::Right | KeyCode::Down => state.split = (state.split + 5).min(90),
                    _ => {}
                }
            }
            Kind::Plot => {
                let full = self
                    .data
                    .get(&p.source)
                    .and_then(|data| super::plot::extent(data, &p));
                if let Some(full) = full {
                    let state = self.local.get_mut(&focus).unwrap();
                    let (lo, hi) = state.plot_domain.unwrap_or(full);
                    let span = hi - lo;
                    let mid = (lo + hi) / 2.;
                    state.plot_domain = match key.code {
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            Some((mid - span / 4., mid + span / 4.))
                        }
                        KeyCode::Char('-') => Some((mid - span, mid + span)),
                        KeyCode::Left => Some((lo - span / 10., hi - span / 10.)),
                        KeyCode::Right => Some((lo + span / 10., hi + span / 10.)),
                        KeyCode::Home => None,
                        _ => state.plot_domain,
                    };
                }
            }
            Kind::Button if key.code == KeyCode::Enter || key.code == KeyCode::Char(' ') => {
                self.activate(&focus)
            }
            _ => {}
        }
        self.dirty = true;
        true
    }
    pub fn paste(&mut self, text: &str) {
        if self
            .nodes
            .get(&self.focus)
            .is_some_and(|(k, _)| *k == Kind::Input)
            && self.enabled(&self.focus)
        {
            let state = self.local.get_mut(&self.focus).unwrap();
            if state.draft.len() + text.len() <= 65536 {
                state.generation += 1;
                if let Some(ms) = self.nodes[&self.focus].1.debounce_ms {
                    state.query_due = Some(Instant::now() + std::time::Duration::from_millis(ms));
                }
                state.draft.insert_str(state.cursor, text);
                state.cursor += text.len();
                state.edited = true;
                self.dirty = true;
            }
        }
    }
    pub fn metrics(&mut self) {
        if self.profile.enabled {
            self.profile.sources = self
                .data
                .iter()
                .map(|(key, data)| (key.clone(), data.revision))
                .collect();
        }
        self.profile.gauge("retained_widgets", self.nodes.len());
        self.profile
            .gauge("cached_rows", self.data.values().map(Data::cached).sum());
        self.profile.gauge("in_flight", self.pending.len());
    }
}
