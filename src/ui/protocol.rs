//! Resolved presentation values only. No executable code crosses this boundary.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const QUEUE_FRAMES: usize = 8;
pub const MAX_NODES: usize = 256;
pub const MAX_ROWS: usize = 10_000;
pub const WINDOW_ROWS: usize = 256;
pub const CACHE_ROWS: usize = 1024;
pub const MAX_IN_FLIGHT: usize = 4;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Panel,
    Text,
    Input,
    Button,
    List,
    Table,
    Toggle,
    Select,
    Split,
    Canvas,
    Plot,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Props {
    pub debounce_ms: Option<u64>,
    pub min_parent_width: u16,
    pub back_action: String,
    pub forward_action: String,
    pub columns: Vec<String>,
    pub options: Vec<String>,
    pub inputs: Vec<String>,
    pub cells: Vec<Glyph>,
    pub x_field: String,
    pub y_field: String,
    pub domain_y: Option<[f64; 2]>,
    pub text: String,
    pub title: String,
    pub value: String,
    pub placeholder: String,
    pub direction: String,
    pub border: bool,
    pub padding: u16,
    pub gap: u16,
    pub size: u16,
    pub focused: bool,
    pub disabled: bool,
    pub hidden: bool,
    pub multiline: bool,
    pub follow_end: bool,
    pub source: String,
    pub field: String,
    /// Local list/detail binding; never asks the model for a selected row again.
    pub detail_of: String,
    /// Button enabled only when the named input contains non-whitespace text.
    pub nonempty: String,
    /// An action includes the named input's current draft.
    pub input: String,
    pub action: String,
    pub clear_on_ack: bool,
    pub style: String,
    pub accessibility_label: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Glyph {
    pub x: u16,
    pub y: u16,
    pub text: String,
    #[serde(default)]
    pub style: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    #[serde(default)]
    pub key: String,
    pub kind: Kind,
    #[serde(default)]
    pub props: Props,
    #[serde(default)]
    pub children: Vec<Node>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub key: String,
    pub fields: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    pub id: String,
    pub revision: u64,
    pub total: usize,
    #[serde(default)]
    pub start: usize,
    pub rows: Vec<Row>,
    #[serde(default = "default_retention")]
    pub retention: usize,
    #[serde(default)]
    pub windowed: bool,
}
fn default_retention() -> usize {
    MAX_ROWS
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Init {
        revision: u64,
        root: Node,
        #[serde(default)]
        collections: Vec<Collection>,
    },
    Tree {
        base_revision: u64,
        revision: u64,
        root: Node,
    },
    Properties {
        base_revision: u64,
        revision: u64,
        updates: Vec<PropertyUpdate>,
    },
    Collection {
        collection: Collection,
    },
    Change {
        source: String,
        base_revision: u64,
        revision: u64,
        changes: Vec<Change>,
    },
    Window {
        source: String,
        revision: u64,
        request_id: u64,
        start: usize,
        total: usize,
        rows: Vec<Row>,
    },
    QueryResult {
        request_id: u64,
        generation: u64,
        #[serde(default)]
        collection: Option<Collection>,
        #[serde(default)]
        message: String,
    },
    Ack {
        request_id: u64,
        ok: bool,
        #[serde(default)]
        message: String,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyUpdate {
    pub key: String,
    pub props: Props,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Append { rows: Vec<Row> },
    Replace { row: Row },
    Remove { key: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Frame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<u64>,
    pub schema_version: u32,
    pub view: String,
    #[serde(flatten)]
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub schema_version: u32,
    pub view: String,
    pub request_id: u64,
    #[serde(flatten)]
    pub intent: IntentKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum IntentKind {
    Query {
        widget: String,
        action: String,
        value: String,
        generation: u64,
    },
    Action {
        widget: String,
        action: String,
        value: Value,
    },
    Window {
        source: String,
        revision: u64,
        start: usize,
        count: usize,
    },
    Resync {
        target: String,
        revision: u64,
    },
}
