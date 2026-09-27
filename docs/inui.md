# Retained Innards UI toolkit

The first toolkit implementation uses the `inui` executable in Innards and the
`UI::*` classes in Trashtalk. The jq compiler and Bash runtime remain canonical.
Widget descriptions, collection rows, and protocol messages are JSON values;
they are not persistent widget objects.

## Try it

Build/install `inui` from the Innards checkout, then compile Trashtalk:

```bash
cd ~/dev/rust/innards
cargo install --path . --bin inui
cd ~/.trashtalk
make bash
source lib/trash.bash
@ UI::Events open
@ UI::Inspector openRecord: '{"order":{"customer":{"name":"Ada"}},"items":[1,2,3]}' title: Order
```

`UI::Events` is a **synthetic live event inspector** and performance workload:
10,000 initial events, sixteen new events per idle poll, selection/details,
debounced filtering, and a small plot. Its events are not agent transcripts or
production telemetry. Existing `inagent` and `ininspect` flows remain available.
Applications connect their own bulk sources through the handler contract below.

Tab/Shift-Tab move focus; arrows, Home/End and PageUp/PageDown navigate lists and
tables. Enter invokes an action or submits a single-line input. Multiline input
uses Enter for newline and Ctrl-Enter for submit where the terminal supports
that chord. Bracketed paste, Unicode cursor movement, Delete and Backspace stay
native. Escape or Ctrl-C detaches. The shared Ctrl-X resize chords work.

Toggle with Space/Enter. Change a select with arrows and commit with Enter.
Drag a split divider or focus it and use arrows. Focus a plot to zoom with `+/-`,
pan with left/right, or reset with Home; mouse movement shows column sample
values. The inspector provides Back/Forward, clickable stack dots, and
Alt-Left/Alt-Right. Narrow terminals show only the active pane; at 80 columns
and above the parent also appears. Up to sixteen entries retain their local
selection and scroll. Drilling an older entry replaces its forward branch.

## Describing a view

Descriptions use ordinary DSL messages and typed JSON literals. The richer
Smalltalk-like examples in the Trashtalk design describe the
intended composition style; they are not additional compiler syntax.

```smalltalk
label := @ UI::Node text: 'Ready' key: 'status'.
input := @ UI::Node input: '' key: 'draft' action: 'send'.
children := #((label jsonValue) (input jsonValue)) asJson.
root := @ UI::Node panel: children key: 'root' direction: 'column'.
frame := @ UI::Surface view: 'example' revision: '0' root: root collections: '[]'.
```

`UI::Node kind:key:props:children:` exposes all resolved native properties.
The primitives are `panel`, `text`, `input`, `button`, `toggle`, `select`,
`list`, `table`, `split`, `canvas`, and `plot`. Common properties include `title`,
`border`, `padding`, `gap`, `size` (fixed cells; zero shares available space),
`hidden`, `disabled`, `focused` (a new/changed focus request), and `style` (`primary`, `error`, or default).
Panels use `direction: 'row'` or `'column'`. `min_parent_width` hides a child
when its parent is too narrow without losing retained identity.

`UI::Form fields:key:submit:` lowers a small array of `{key,label,value}` string
fields into labels, inputs and a submit button. A button's `inputs` property
names controls to gather into one submitted object; toggles produce booleans.
For richer forms, compose controls directly. Application handlers own domain
validation. Native local rules are deliberately small:

- `nonempty: 'input-key'` enables a button only for a nonblank native draft.
- `input: 'input-key'` includes that input's current text in the action.
- `clear_on_ack: true` clears a submitted draft only on success, and only if it
  still equals the sent draft. Failed or uncertain actions retain it.
- `detail_of: 'list-key'` plus `field` displays a selected cached row's field.
- `debounce_ms` on an input enables read-only queries after 50–10,000 ms idle.
  At most one query per input is outstanding; later edits replace the desired
  query. A result must match both request ID and native edit generation.

An edited input ignores model `value` replacements. Change the widget key to
explicitly replace an editing session. A keyed node preserves local state on
reordering; a type change replaces it. Unkeyed nodes use parent/position identity.
The implementation has basic terminal styling and text editing, not a general
CSS system, full editor command set, or automatic system clipboard integration.

## Signals and ordinary blocks

Signals are scoped to an open surface. The bridge supplies their temporary
storage, which is removed on detach. Small view-local files cross Bash's
command-substitution boundaries; value reads, writes and dependency capture use
Bash builtins. They do not write the object database.

```smalltalk
@ UI::Signal set: 'refreshAllowed' to: 'true'.
enabled := @ UI::Binding named: 'refresh-button' block: [
  @ UI::Signal value: 'refreshAllowed'
].
@ UI::Binding clearInvalidations.
```

After changing a signal, `UI::Binding invalidated` returns dependent binding
names. Evaluate those names with `UI::Binding evaluate:` and include the resolved
values in one property batch, then call `clearInvalidations`. Setting an equal
value does not invalidate. `UI::Signal invalidate:` supports explicit changes
outside a signal. Branching blocks replace their captured dependency set after
successful evaluation. Bindings are a read-only application contract; the
signal setters reject writes during capture, but arbitrary user-defined Bash
side effects are not sandboxed. Bindings never run from native paint or editing.

## Handler and transport contract

Open a handler with `@ UI::Surface open: HandlerClass context: contextJson`.
For explicit executable arguments, use `openArgv:handler:context:`. Exact argv,
stdin JSONL, stdout JSONL and `/dev/tty` drawing reuse the session-scoped Tool
bridge. There is no global daemon or native Trashtalk runtime.

Implement three ordinary class/instance messages:

| Message | Result |
| --- | --- |
| `frameFor: context` | One initial `init` frame. |
| `handleFrame: intent context: context` | `{context,frames:[...]}`; context may be omitted to retain it. |
| `pollFor: context` | Same result; `{frames:[]}` for an unchanged model. Called after one idle second. |

The application validates action names, widget identity, values, authorization,
and current domain state. It owns database transactions and durable action
idempotence. Read an entire requested range in one snapshot; avoid per-row
object sends or serializers. `asJson`, `jsonRows:into:` and bulk domain APIs
support this without adding language syntax.

The bridge remembers eight recent responses. A duplicate ID reuses its response;
an older evicted ID receives an unknown-outcome rejection and is never executed
again. Detach never controls an agent or replays an unacknowledged mutation.
A closed pipe leaves the native cached view readable. Recovery on the same pipe
uses `init` or a collection descriptor; starting a new process starts a new view
and does not recover native drafts automatically.

## Protocol version 1

Every frame has `schema_version:1` and a nonempty `view` identity, pinned by the
first `init`. Responses can set `caused_by` to a request ID for diagnostics.
Tree revision and each collection's revision are independent integers. Rows use
nonempty stable keys unique in their collection and resolved string fields.
Order is explicit in the array; appends go at the end and removals retain the
relative order of survivors.

| Application → Innards `type` | Required payload |
| --- | --- |
| `init` | `revision`, `root`, `collections` (optional). Authoritative view snapshot. |
| `tree` | `base_revision`, `revision`, `root`. Reconcile a replacement description. |
| `properties` | `base_revision`, `revision`, `updates:[{key,props}]`. Each supplied `props` replaces that node's entire property set. |
| `collection` | `collection:{id,revision,total,start,rows,retention,windowed}`. Complete bounded data or a large-source descriptor with an optional initial cache range. |
| `change` | `source`, `base_revision`, `revision`, `changes`. Operations: `{op:"append",rows}`, `{op:"replace",row}`, `{op:"remove",key}`. |
| `window` | `source`, `revision`, `request_id`, `start`, `total`, `rows`. Must match the requested range/revision. |
| `query_result` | `request_id`, `generation`, optional `collection` and `message`. Stale results are discarded before applying data. |
| `ack` | `request_id`, `ok`, optional `message`. Does not imply an update has rendered. |

`init` and `collection` may replace a snapshot at the current revision during
resynchronization; an older collection revision is rejected. Delta revisions
must increase and their base must match. A whole batch validates before mutation.
Mismatch requests `resync`; the current consistent view remains displayed.
For changing windowed data, publish a fresh descriptor at the new revision;
old cached ranges and pending reads are invalidated. Windows cannot independently
reorder rows at one revision. A new desired viewport supersedes an old reply;
the next needed request is issued after the outstanding read settles.

| Innards → application `intent` | Payload in addition to positive increasing `request_id` |
| --- | --- |
| `action` | `widget`, `action`, `value` (string, boolean, selected row, form object or null). |
| `query` | `widget`, `action`, `value` (string), `generation`. Read-only. |
| `window` | `source`, `revision`, `start`, `count` (256). |
| `resync` | `target` (`view` or collection ID), current `revision`. |

A full list has `start:0`, `total == rows.length`, and `windowed:false`.
For a large source, set `windowed:true` and advertise its total without loading
all records. Cache misses request 256-row aligned blocks, including a neighboring
prefetch margin. Cached scrolling and resizing make no Bash calls. Plots require
a complete bounded sampling collection; supply a separate downsampled source
for plots over unbounded history.

The wire records are bounded:

| Resource | Limit |
| --- | --- |
| JSONL record, including newline | 2 MiB |
| Handler response before decode | 2 MiB, at most 8 frames |
| Bridge context | 64 KiB |
| Native incoming/outgoing channel | 8 records each, plus one active reader/writer record |
| Outstanding requests | 4 overall; one window per source, one query per input, one action per widget |
| Widget tree | 256 nodes, depth below 32; 64 KiB encoded properties per node |
| Collections | 16 |
| Complete collection | 10,000 retained rows, at most 8 MiB field/key bytes |
| Window cache | 1,024 rows per source |
| Row | 8 KiB field/key bytes, at most 64 fields |
| Change batch | 256 operations, at most 256 appended rows |
| Native draft | 64 KiB |
| Table / select / canvas | 32 columns / 128 options / 4,096 cell commands |

A reader/writer thread owns blocking pipe I/O. The terminal thread only tries
bounded sends and drains a bounded number of incoming frames per loop. Local
input wakes drawing immediately; background updates draw at most sixty times
per second. Queue saturation preserves pending actions and coalesces desired
reads. Collection retention evicts oldest rows and increments a loss counter.
Producers must trim or resynchronize if a byte limit would be exceeded.

## Profiling and validation

Native profiling is disabled by default. `inui --profile /tmp/native.json`
enables counters, bounded latency histograms and the last 32 request receipts.
F12 writes a current summary; F11 resets native counters; exit also writes it.
`TRASHTALK_UI_PROFILE=/tmp/bash.json` enables the bridge summary at detach.
Use an argv override to enable both, for example:

```bash
export TRASHTALK_UI_PROFILE=/tmp/ui-bash.json
@ UI::Surface openArgv: '["inui","--profile","/tmp/ui-native.json"]' \
  handler: UI::Events context: '{"count":10000,"revision":0}'
```

Native schema 1 includes `view`, `counters`, `sources` (latest revisions),
`timings`, and bounded `receipts`. Timing histograms use microsecond upper bounds
100, 500, 1,000, 4,000, 16,667, 50,000, 250,000, infinity. They include count,
total and maximum. Receipts distinguish acknowledgement from rendered-update
latency when the handler supplies `caused_by`. Request timing starts at admission,
so it includes native queueing. All native durations use `Instant`.

`draw_and_terminal_write` includes Ratatui diffing/output; the nested
`buffer_render_including_layout` and `layout` measurements isolate native work.
The current backend does not expose a pure OS-write-only timer. Bash reports
handler distributions, bounded request receipts, binding evaluation totals/max,
bridge response preparation, bytes, and explicitly instrumented bridge jq
calls. Binding summaries use a fixed-size temporary record because evaluation
crosses command substitutions. Bash uses `EPOCHREALTIME` (wall time, clamped at
zero), with coarse `SECONDS` fallback; it never spawns a clock utility. Routine
counters are **not** an OS process census or complete database profiler.

Run `bin/trash-bench-ui` for dedicated observer runs at 100 and 10,000 rows. It
checks row count and endpoint content, checks the incremental batch, and counts
jq/sqlite executions plus Bash processes. PATH wrappers affect every reported
run; expensive DEBUG process tracing is enabled only in the labeled traced run.
Native release measurements and the live-terminal delayed-handler test are
reproducible in the Innards checkout:

```bash
cargo test --release --lib ui::tests::performance_receipt -- --ignored --nocapture
cargo test --test ui_contract
```

See [performance receipts](inui-performance.md) for measured results and
limits of those measurements.
