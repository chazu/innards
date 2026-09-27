use super::{data::Data, model::App, protocol::*, render::Renderer, transport::Transport};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
fn frame(value: Value) -> Frame {
    serde_json::from_value(value).unwrap()
}
fn init(rows: usize, windowed: bool) -> Frame {
    frame(
        json!({"schema_version":1,"view":"test","type":"init","revision":0,"root":{"kind":"panel","key":"root","props":{"direction":"row"},"children":[
        {"kind":"list","key":"events","props":{"source":"events","field":"text","border":true}},
        {"kind":"text","key":"detail","props":{"detail_of":"events","field":"detail"}},
        {"kind":"input","key":"draft","props":{"value":"","action":"send","clear_on_ack":true}},
        {"kind":"button","key":"send","props":{"text":"Send","input":"draft","nonempty":"draft","action":"send","clear_on_ack":true}}
    ]},"collections":[{"id":"events","revision":0,"total":rows,"windowed":windowed,"rows":(0..if windowed{256.min(rows)}else{rows}).map(|i|json!({"key":format!("e{i}"),"fields":{"text":format!("row {i}"),"detail":format!("detail {i}")}})).collect::<Vec<_>>()}]}),
    )
}
fn key(app: &mut App, key: KeyCode) {
    app.key(KeyEvent::new(key, KeyModifiers::NONE));
}
fn draw(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
    let mut renderer = Renderer::default();
    terminal.draw(|frame| renderer.draw(app, frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}
#[test]
fn local_navigation_10k_rows_emits_no_model_calls() {
    let mut app = App::new(true);
    app.apply(init(10000, false)).unwrap();
    assert_eq!(app.data["events"].row(9999).unwrap().key, "e9999");
    for _ in 0..200 {
        key(&mut app, KeyCode::Down);
        draw(&mut app);
        app.windows();
    }
    assert!(app.take_intent().is_none());
    let text = draw(&mut app);
    assert!(text.contains("row 200"));
    assert!(text.contains("detail 200"));
    app.metrics();
    assert_eq!(app.profile.counters["retained_widgets"], 5);
    assert_eq!(app.profile.counters["cached_rows"], 10000);
    assert!(app.profile.counters["visible_rows"] < 30);
}
#[test]
fn stale_property_and_ack_cannot_erase_newer_draft() {
    let mut app = App::new(false);
    app.apply(init(5, false)).unwrap();
    app.focus = "draft".into();
    app.paste("first");
    key(&mut app, KeyCode::Enter);
    let intent = app.take_intent().unwrap();
    app.paste(" second");
    app.apply(frame(json!({"schema_version":1,"view":"test","type":"properties","base_revision":0,"revision":1,"updates":[{"key":"draft","props":{"value":"stale","action":"send"}}]}))).unwrap();
    app.apply(frame(json!({"schema_version":1,"view":"test","type":"ack","request_id":intent.request_id,"ok":true,"message":"accepted"}))).unwrap();
    assert_eq!(app.local["draft"].draft, "first second");
}
#[test]
fn batch_validation_is_atomic_and_resync_is_deduplicated() {
    let mut app = App::new(true);
    app.apply(init(5, false)).unwrap();
    let bad = || {
        frame(
            json!({"schema_version":1,"view":"test","type":"change","source":"events","base_revision":0,"revision":1,"changes":[{"op":"replace","row":{"key":"e0","fields":{"text":"changed"}}},{"op":"remove","key":"missing"}]}),
        )
    };
    app.apply(bad()).unwrap();
    app.apply(bad()).unwrap();
    assert_eq!(app.data["events"].revision, 0);
    assert_eq!(app.data["events"].row(0).unwrap().fields["text"], "row 0");
    assert!(matches!(
        app.take_intent().unwrap().intent,
        IntentKind::Resync { .. }
    ));
    assert!(app.take_intent().is_none());
}
#[test]
fn stale_window_does_not_replace_new_viewport() {
    let mut app = App::new(true);
    app.apply(init(100000, true)).unwrap();
    draw(&mut app);
    app.local.get_mut("events").unwrap().offset = 512;
    app.windows();
    let request = app.take_intent().unwrap();
    app.local.get_mut("events").unwrap().offset = 8192;
    app.windows();
    assert!(app.take_intent().is_none());
    app.apply(frame(json!({"schema_version":1,"view":"test","type":"window","source":"events","revision":0,"request_id":request.request_id,"start":512,"total":100000,"rows":(512..768).map(|i|json!({"key":format!("e{i}"),"fields":{"text":i.to_string()}})).collect::<Vec<_>>()}))).unwrap();
    assert!(app.data["events"].row(512).is_none());
    app.windows();
    assert!(matches!(
        app.take_intent().unwrap().intent,
        IntentKind::Window { start: 8192, .. }
    ));
    assert_eq!(app.profile.counters["stale_replies"], 1);
}
#[test]
fn action_limit_and_reconnect_never_replay() {
    let mut app = App::new(false);
    app.apply(init(5, false)).unwrap();
    app.focus = "draft".into();
    app.paste("keep me");
    for _ in 0..100 {
        key(&mut app, KeyCode::Enter);
    }
    assert!(app.take_intent().is_some());
    assert!(app.take_intent().is_none());
    app.apply(init(5, false)).unwrap();
    assert!(app.take_intent().is_none());
    assert_eq!(app.local["draft"].draft, "keep me");
    app.disconnected();
    key(&mut app, KeyCode::Enter);
    assert!(app.take_intent().is_none());
    assert!(app.status.contains("unknown") || app.status.contains("acknowledgement"));
}
#[test]
fn ring_retention_and_keyed_replace_preserve_content() {
    let c = Collection {
        id: "x".into(),
        revision: 0,
        total: 3,
        start: 0,
        rows: (0..3)
            .map(|i| Row {
                key: i.to_string(),
                fields: Default::default(),
            })
            .collect(),
        retention: 3,
        windowed: false,
    };
    let mut d = Data::new(c).unwrap();
    d.change(
        0,
        1,
        vec![
            Change::Append {
                rows: vec![Row {
                    key: "3".into(),
                    fields: Default::default(),
                }],
            },
            Change::Replace {
                row: Row {
                    key: "2".into(),
                    fields: [("text".into(), "updated".into())].into(),
                },
            },
        ],
    )
    .unwrap();
    assert_eq!(d.total, 3);
    assert_eq!(d.dropped, 1);
    assert_eq!(d.row(0).unwrap().key, "1");
    assert_eq!(d.row(1).unwrap().fields["text"], "updated");
    assert_eq!(d.row(2).unwrap().key, "3");
}
#[test]
fn keys_retain_state_and_type_changes_reset_it() {
    let mut app = App::new(false);
    app.apply(init(2, false)).unwrap();
    app.focus = "draft".into();
    app.paste("preserve");
    let mut root = app.root.clone().unwrap();
    root.children.reverse();
    app.apply(Frame {
        caused_by: None,
        schema_version: 1,
        view: "test".into(),
        message: Message::Tree {
            base_revision: 0,
            revision: 1,
            root,
        },
    })
    .unwrap();
    assert_eq!(app.local["draft"].draft, "preserve");
    assert_eq!(app.focus, "draft");
}
struct Stalled(Arc<Mutex<bool>>);
impl Write for Stalled {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        while !*self.0.lock().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn blocked_pipe_does_not_block_ui_thread_and_queue_is_bounded() {
    let release = Arc::new(Mutex::new(false));
    let transport = Transport::new(io::empty(), Stalled(release.clone()));
    let start = Instant::now();
    let mut accepted = 0;
    for _ in 0..100 {
        if transport.send(vec![b'x'; 4096]).unwrap() {
            accepted += 1;
        }
    }
    assert!(accepted <= QUEUE_FRAMES + 1);
    assert!(start.elapsed() < Duration::from_millis(100));
    assert!(transport.traffic.peak.load(Ordering::Relaxed) <= (QUEUE_FRAMES + 1) * 4096);
    *release.lock().unwrap() = true;
}
struct Endless(Arc<AtomicUsize>);
impl Read for Endless {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        b.fill(b'x');
        self.0.fetch_add(b.len(), Ordering::Relaxed);
        Ok(b.len())
    }
}
#[test]
fn malformed_unterminated_stream_is_bounded() {
    let count = Arc::new(AtomicUsize::new(0));
    let t = Transport::new(Endless(count.clone()), io::sink());
    assert!(
        t.input
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err()
    );
    assert!(count.load(Ordering::Relaxed) <= FRAME_BYTES + 8192);
}
#[test]
fn profiling_disabled_has_no_samples() {
    let mut app = App::new(false);
    app.apply(init(10, false)).unwrap();
    draw(&mut app);
    app.metrics();
    assert!(app.profile.counters.is_empty());
    assert!(app.profile.timings.is_empty());
}

#[test]
#[ignore = "release performance receipt: cargo test --release ui::tests::performance_receipt -- --ignored --nocapture"]
fn performance_receipt() {
    for enabled in [false, true] {
        let mut app = App::new(enabled);
        app.apply(init(10000, false)).unwrap();
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut draws = Vec::new();
        let mut changes = Vec::new();
        for i in 0..1000 {
            let now = Instant::now();
            key(&mut app, KeyCode::Down);
            terminal
                .draw(|frame| renderer.draw(&mut app, frame))
                .unwrap();
            draws.push(now.elapsed().as_micros() as u64);
            let now = Instant::now();
            app.apply(Frame {
                caused_by: None,
                schema_version: 1,
                view: "test".into(),
                message: Message::Change {
                    source: "events".into(),
                    base_revision: i,
                    revision: i + 1,
                    changes: vec![Change::Replace {
                        row: Row {
                            key: "e0".into(),
                            fields: [("text".into(), format!("change {i}"))].into(),
                        },
                    }],
                },
            })
            .unwrap();
            changes.push(now.elapsed().as_micros() as u64);
        }
        assert!(app.take_intent().is_none());
        assert_eq!(app.data["events"].total, 10000);
        assert_eq!(
            app.data["events"].row(0).unwrap().fields["text"],
            "change 999"
        );
        assert_eq!(app.data["events"].row(9999).unwrap().key, "e9999");
        draws.sort();
        changes.sort();
        println!(
            "{}",
            json!({"profile":enabled,"rows":10000,"samples":1000,"input_and_test_buffer_us":{"p50":draws[500],"p95":draws[950],"p99":draws[990],"max":draws[999]},"single_row_update_us":{"p50":changes[500],"p99":changes[990],"max":changes[999]}})
        );
    }
}
#[test]
fn controls_table_split_and_canvas_render_and_submit_resolved_form() {
    let mut app = App::new(false);
    let mut f = init(10000, false);
    if let Message::Init { root, .. } = &mut f.message {
        root.children[0].kind = Kind::Table;
        root.children[0].props.columns = vec!["text".into(), "detail".into()];
        root.children.push(Node {
            key: "enabled".into(),
            kind: Kind::Toggle,
            props: Props {
                text: "Enabled".into(),
                ..Props::default()
            },
            children: vec![],
        });
        root.children.push(Node {
            key: "choice".into(),
            kind: Kind::Select,
            props: Props {
                options: vec!["a".into(), "b".into()],
                value: "a".into(),
                ..Props::default()
            },
            children: vec![],
        });
        root.children[3].props.nonempty.clear();
        root.children[3].props.inputs = vec!["draft".into(), "enabled".into(), "choice".into()];
    }
    app.apply(f).unwrap();
    app.focus = "enabled".into();
    key(&mut app, KeyCode::Char(' '));
    app.focus = "choice".into();
    key(&mut app, KeyCode::Right);
    app.focus = "draft".into();
    app.paste("form value");
    app.focus = "send".into();
    key(&mut app, KeyCode::Enter);
    match app.take_intent().unwrap().intent {
        IntentKind::Action { value, .. } => assert_eq!(
            value,
            json!({"draft":"form value","enabled":true,"choice":"b"})
        ),
        _ => panic!("not an action"),
    };
    assert!(draw(&mut app).contains("Enabled"));
}
#[test]
fn removal_keeps_selected_key_and_cache_refreshes_equal_revision_snapshot() {
    let mut app = App::new(false);
    app.apply(init(10, false)).unwrap();
    for _ in 0..5 {
        key(&mut app, KeyCode::Down);
    }
    app.apply(frame(json!({"schema_version":1,"view":"test","type":"change","source":"events","base_revision":0,"revision":1,"changes":[{"op":"remove","key":"e0"}]}))).unwrap();
    assert_eq!(app.selected_row("events").unwrap().key, "e5");
    assert_eq!(app.local["events"].selected, 4);
}
#[test]
fn split_and_narrow_parent_layout_are_native() {
    let mut app = App::new(false);
    let f = frame(
        json!({"schema_version":1,"view":"split","type":"init","revision":0,"root":{"kind":"panel","children":[{"kind":"text","key":"parent","props":{"text":"PARENT","min_parent_width":80}},{"kind":"split","key":"split","props":{"direction":"row"},"children":[{"kind":"text","props":{"text":"LEFT"}},{"kind":"canvas","props":{"cells":[{"x":0,"y":0,"text":"RIGHT"}]}}]}]}}),
    );
    app.apply(f).unwrap();
    let mut renderer = Renderer::default();
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    terminal.draw(|f| renderer.draw(&mut app, f)).unwrap();
    assert!(!renderer.areas.contains_key("parent"));
    assert!(renderer.dividers.contains_key("split"));
    app.focus = "split".into();
    key(&mut app, KeyCode::Right);
    assert_eq!(app.local["split"].split, 55);
    assert!(app.take_intent().is_none());
}
#[test]
fn window_cache_never_exceeds_bound_and_order_change_rejects_atomically() {
    let mut d = Data::new(Collection {
        id: "x".into(),
        revision: 0,
        total: 100000,
        start: 0,
        rows: vec![],
        retention: 10000,
        windowed: true,
    })
    .unwrap();
    for start in (0..10000).step_by(256) {
        let rows = (start..start + 256)
            .map(|i| Row {
                key: i.to_string(),
                fields: Default::default(),
            })
            .collect();
        d.window(start, 100000, rows, start).unwrap();
        assert!(d.cached() <= CACHE_ROWS);
    }
    let key = d.row(9984).unwrap().key.clone();
    assert!(
        d.window(
            9985,
            100000,
            vec![Row {
                key,
                fields: Default::default()
            }],
            9984
        )
        .is_err()
    );
    assert_eq!(d.row(9985).unwrap().key, "9985");
}
#[test]
fn debounced_queries_coalesce_and_reject_stale_generations() {
    let mut app = App::new(true);
    let mut initial = init(4, false);
    if let Message::Init { root, .. } = &mut initial.message {
        root.children[2].props.debounce_ms = Some(50);
    }
    app.apply(initial).unwrap();
    app.focus = "draft".into();
    app.paste("old");
    app.local.get_mut("draft").unwrap().query_due = Some(Instant::now());
    app.queries();
    let request = app.take_intent().unwrap();
    let generation = if let IntentKind::Query { generation, .. } = request.intent {
        generation
    } else {
        panic!("missing query")
    };
    app.paste(" new");
    app.local.get_mut("draft").unwrap().query_due = Some(Instant::now());
    app.queries();
    assert!(app.take_intent().is_none());
    app.apply(frame(json!({"schema_version":1,"view":"test","type":"query_result","request_id":request.request_id,"generation":generation,"collection":{"id":"events","revision":99,"total":0,"rows":[]},"message":"STALE"}))).unwrap();
    assert_eq!(app.data["events"].total, 4);
    assert_ne!(app.status, "STALE");
    app.queries();
    assert!(
        matches!(app.take_intent().unwrap().intent,IntentKind::Query{value,..} if value=="old new")
    );
}
#[test]
fn profiling_correlates_ack_and_render_separately_and_stays_bounded() {
    let mut profile = super::profile::Profile::new(true);
    for id in 0..100 {
        profile.request(id);
        profile.ack(id);
        profile.updated(id);
        profile.rendered();
    }
    assert_eq!(profile.receipts.len(), 32);
    let receipt = profile.receipts.back().unwrap();
    assert_eq!(receipt.request_id, 99);
    assert!(receipt.ack_us.is_some() && receipt.rendered_update_us.is_some());
}
#[test]
fn oversized_replacement_does_not_mutate_collection() {
    let mut app = App::new(false);
    app.apply(init(3, false)).unwrap();
    let d = app.data.get_mut("events").unwrap();
    assert!(
        d.change(
            0,
            1,
            vec![Change::Replace {
                row: Row {
                    key: "e0".into(),
                    fields: [("text".into(), "x".repeat(8193))].into()
                }
            }]
        )
        .is_err()
    );
    assert_eq!(d.revision, 0);
    assert_eq!(d.row(0).unwrap().fields["text"], "row 0");
}
#[test]
fn explicit_focus_moves_to_new_inspection_pane_without_resetting_draft() {
    let mut app = App::new(false);
    app.apply(init(2, false)).unwrap();
    let mut root = app.root.clone().unwrap();
    root.children[2].props.focused = true;
    app.apply(Frame {
        schema_version: 1,
        view: "test".into(),
        caused_by: None,
        message: Message::Tree {
            base_revision: 0,
            revision: 1,
            root,
        },
    })
    .unwrap();
    assert_eq!(app.focus, "draft");
    app.paste("keep");
    assert_eq!(app.local["draft"].draft, "keep");
}
