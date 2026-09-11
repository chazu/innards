# innards

Small inline terminal tools for jumping around source code and editing files without
clearing the shell above them.

On account of agents I spend most of my time in a terminal window these days,
and often also remote over SSH, and I missed some of the conveniences of an IDE
but wanted something quick and nimble that didn't break flow.

These little utilities let you drop in and out of navigation and editing and
viewing files but what you were working on visible.

Quick, and get out of your way.

## Binaries

- `navsplat`: rust-analyzer-backed Rust workspace symbol picker.
- `inmacs`: inline editor with Emacs-like navigation and editing keys.
- `inpage`: read-only inline pager with the same movement/search surface as
  `inmacs`.
- `inpick`: generic JSON Lines picker with source previews and structured
  selection results.
- `ininspect`: navigable JSON object tree with typed leaf-edit proposals.
- `indiff`: presentation-only unified-diff reviewer with explicit structured
  accept, reject, and cancel outcomes.
- `inagent`: live agent conversation with backlog, a message composer, search,
  and explicit session-control intents over a duplex JSONL protocol.

All seven use ratatui with an inline terminal viewport, so they open below the
current prompt instead of taking over the whole screen.

While any view is open, use **Ctrl-X, then ^** (`C-x ^`) to grow it by one
terminal row, or **Ctrl-X, then -** (`C-x -`) to shrink it by one row.
Alt-Down and Alt-Up are equivalent. Resizing stops at the view's minimum
usable height and the terminal's full height; it preserves the current
selection, search, and edits. The size applies to the currently open view.

## Build

```sh
cargo build --release --bins
```

The binaries will be under `target/release/`.

## inagent

The caller streams version-1 `snapshot` and `ack` records to stdin. `inagent`
renders through `/dev/tty` and flushes explicit intents to stdout; it owns no
agent process, database, or delivery policy. In Trashtalk, open it with
`@ AgentSession browse` → **Attach to conversation**, or `@ "$session" focus`.

Long lines wrap to the composer width and scroll to keep the cursor visible.
Enter inserts a newline; **C-c C-c** sends the draft. **Tab** switches between
composer and transcript. **C-n/C-p**, **C-v/M-v**, **C-s/C-r**, and **M-</M->**
navigate, search, load earlier history, or follow live output. **M-x** opens
the command menu. **C-x C-c** detaches, asking before discarding a draft.
Ctrl-X resize chords and Alt-Up/Down work as in the other applets.

The applet preserves drafts until send acknowledgement, keeps a scrolled
transcript anchored as updates arrive, and emits read intents only for visible
message entries. Stop confirmation captures the displayed run ID. Detach and
signals never emit a stop intent. A disconnected bridge leaves the loaded
backlog available and disables sending.

```json
{"schema_version":1,"type":"snapshot","session":{"id":"s","title":"Gusgus","workspace":"/repo","profile":"jcode","lifecycle":"open","activity":"running","run_id":"r","pending":0},"entries":[{"id":"entry-1","kind":"assistant","title":"Assistant","text":"Working on the parser."}],"has_earlier":false,"window":400}
{"schema_version":1,"type":"ack","request_id":1,"ok":true,"message":"Message sent"}
```

Intents contain `schema_version`, a connection-local numeric `request_id`, and
`intent`: `send_message` (+ `body`), `mark_viewed` (+ `message_ids`),
`load_older`, `pause_session`, `resume_session`, `interrupt_run` (+ `run_id`), or
`dismiss`. The caller validates every intent against current domain state.

## Install

Install from a local checkout:

```sh
cargo install --path .
```

Install directly from git:

```sh
cargo install --git https://github.com/rdaum/innards.git
```

`cargo install` places the binaries in Cargo's bin directory, usually
`~/.cargo/bin`. Make sure that directory is on `PATH`.

## Requirements

`navsplat` starts `rust-analyzer` and talks to it over LSP, so `rust-analyzer`
must be on `PATH`.

`navsplat` opens selections with `$VISUAL`, then `$EDITOR`, then `vi` if neither
environment variable is set. It invokes the editor as:

```sh
$EDITOR +LINE FILE
```

Clipboard copy tries `wl-copy`, `xclip`, `xsel`, `pbcopy`, then OSC 52.

## navsplat

Run the interactive picker from inside a Rust project:

```sh
navsplat
```

Start with an initial query:

```sh
navsplat pick '#main'
```

Use a specific workspace root or editor:

```sh
navsplat --root ~/src/my-crate --editor 'vim' pick HashMap
```

Print matches without opening the TUI:

```sh
navsplat symbols '#main'
```

Options:

```text
--root PATH       Workspace root. Defaults to the nearest Cargo.toml or .git.
--editor CMD      Editor command. Defaults to $VISUAL, $EDITOR, then vi.
--height ROWS     Inline picker height. Defaults to 20.
```

Picker keys:

```text
Enter             Open the selected symbol, or promote a selected side-pane hit
Esc, Ctrl-C       Quit
q                 Quit when the search input is empty
Up/Down           Move selection
Ctrl-P/Ctrl-N     Move selection
PageUp/PageDown   Move by larger steps
Shift-Up/Down     Scroll the preview pane
Tab               Switch focus between symbols and the side pane
Alt-R             Show references
Alt-C             Show callers
Alt-E             Show callees
Alt-S             Return the right pane to source preview mode
Backspace         Pop back after promoting a side-pane hit
Alt-Y             Copy the selected location
```

The preview is centered around the selected symbol when possible. References,
callers, and callees are loaded lazily for the current selection.

## inmacs

Open a file in the inline editor:

```sh
inmacs src/lib.rs
```

Open at a line:

```sh
inmacs +120 src/lib.rs
inmacs --line 120 src/lib.rs
```

Open at a line and column, override the presentation, or select a syntax:

```sh
inmacs --line 120 --column 8 --title 'Counter>>increment' \
  --status 'Fix the compiler error' --syntax trashtalk src/Counter.trash
```

Stdin-backed editing requires an explicit output destination:

```sh
printf 'Counter subclass: Object\n' |
  inmacs --stdin --output Counter.trash --result-json
```

Set the inline viewport height:

```sh
inmacs --height 18 src/lib.rs
```

Report the editor outcome as one JSON record on stdout. The interface itself
still renders on the controlling terminal, so callers can capture the result:

```sh
result=$(inmacs --result-json src/lib.rs)
```

The process exits 0 after a save or an unchanged close, 3 when dirty edits are
discarded, and 130 when cancelled with Ctrl-C or interrupted by a termination
signal. The result distinguishes `saved`, `unchanged`, `discarded`, and
`cancelled` outcomes.

Pass a versioned annotation document to display diagnostics beside their source
lines without inserting comments into the file:

```json
{
  "schema_version": 1,
  "annotations": [
    {"line": 12, "column": 3, "severity": "error", "message": "expected ]"}
  ]
}
```

```sh
inmacs --annotations diagnostics.json --syntax trashtalk Counter.trash
```

Saves use a flushed temporary file and atomic rename. Existing permission bits
are retained. If the target changed after it was opened, the first save warns
and a second consecutive Ctrl-X Ctrl-S confirms the overwrite. Editable
symlinks are rejected; open their target explicitly. Because an atomic save
replaces the inode, ownership may become the editor user's and extended
attributes are not currently copied.

Core keys:

```text
Ctrl-X Ctrl-S     Save
Ctrl-X Ctrl-C     Quit
Ctrl-X ^          Grow the inline viewport by one row
Ctrl-X -          Shrink the inline viewport by one row
Ctrl-S            Incremental search forward
Ctrl-R            Incremental search backward
Ctrl-S/Ctrl-R     Repeat search while searching
Enter             Finish search while searching
Esc, Ctrl-G       Cancel search while searching
Ctrl-G            Cancel active mark outside search
Ctrl-A/Ctrl-E     Start/end of line
Ctrl-B/Ctrl-F     Character left/right
Alt-B/Alt-F       Word left/right
Alt-Q             Fill/reflow the current paragraph to 80 columns
Ctrl-Left/Right   Word left/right
Ctrl-P/Ctrl-N     Line up/down
Alt-V/Ctrl-V      Page up/down
PageUp/PageDown   Page up/down
Alt-Up/Down       Shrink/grow the inline viewport by one row
Ctrl-Space        Set or clear mark
Ctrl-W            Kill active region
Alt-W             Copy active region
Ctrl-Y            Yank
Ctrl-K            Kill to end of line
Ctrl-D/Delete     Delete character
Backspace         Delete backward
Enter             Insert newline and copy the current indentation
Tab               Advance to the next configured tab stop
Ctrl-/ Ctrl-_     Undo
Ctrl-7            Undo
Ctrl-?            Redo, where the terminal reports it distinctly
```

`inmacs` uses `ropey` internally for text storage and `syntect` for syntax
highlighting. `.trash` files select the bundled Trashtalk syntax and a two-space
tab width by default; `--tab-width` overrides it.

## inpage

Open a read-only inline pager:

```sh
inpage src/lib.rs
inpage +120 src/lib.rs
```

`inpage` accepts the same `--height`, `--line`, and `+LINE` arguments as
`inmacs`. Editing keys are disabled, but movement and search keys are shared.
The mouse wheel scrolls three lines per tick when the pointer is over the pager.
Mouse capture is released when the pager exits. To select text with the mouse,
use your terminal's mouse-reporting override (usually Shift-drag).

It can also read piped content while keeping terminal drawing separate from
stdout:

```sh
result=$(printf 'one\ntwo\n' | inpage --stdin --result-json)
```

`-` is an alias for `--stdin`. JSON output reports `closed` after a normal quit
or `cancelled` after Ctrl-C or a termination signal. `inpage` also accepts the
presentation, syntax, and annotation options described above.

Additional pager quit keys:

```text
Esc
q
```

## inpick

`--preview-hook /path/to/executable` notifies the caller after a readable preview
has appeared, once per candidate per picker invocation. The executable receives
the candidate as JSON on stdin; it may return a `display` object's contents as
JSON on stdout to update the row immediately, or leave stdout empty. It runs
directly, without a shell. Failed hooks end the picker with an error after
restoring the terminal. The caller owns any state change, such as marking an
Inbox message read. Unseen candidates and failed preview reads do not invoke it.
Notifications still apply when the user later cancels the picker.

Pass `--ctrl-d-action archive` to let Ctrl-D return the highlighted candidate
with `"action":"archive"` in the selected result. The caller performs the
action; inpick does not mutate records. Enter selects normally and Escape
cancels. The optional action is shown in the footer.

`inpick` accepts one versioned candidate record per line on stdin. Paths may be
absolute or relative to `--root`; the selected file is previewed around its
one-based line and column.

```sh
printf '%s\n' \
  '{"schema_version":1,"id":"Array>>at:put:","path":"trash/Array.trash","line":49,"column":3,"label":"Array>>at:put:","kind":"instance_method","detail":"DSL"}' |
  inpick --root "$HOME/.trashtalk" --title 'Trashtalk symbols' --result-json
```

The result is a single JSON object. Enter returns `selected` with the original
candidate record and exit status 0. Esc, Ctrl-C, or a termination signal returns
`cancelled` with no selection and exit status 130. The terminal UI writes only
to the controlling terminal, leaving stdout clean for the result.

For messages and other records whose path is only a preview source, add an
optional `display` object:

```json
"display": {
  "prefix": "● Gusgus  14:32",
  "preview_title": "Message",
  "search_text": "full sender address and message body"
}
```

These records show the prefix and label in a compact row, hide the path and
kind, and give more space to the preview. Prefix and search text participate
in filtering; selection still returns the complete original record. Omit
`display` for the existing source-navigation layout.

Picker keys:

```text
Enter             Select the current record
Esc, Ctrl-C       Cancel
Up/Down           Move selection
Ctrl-P/Ctrl-N     Move selection
PageUp/PageDown   Move by larger steps
Shift-Up/Down     Scroll the source preview
Typing/Backspace  Filter across id, label, kind, detail, and path
```

Library consumers can provide candidates through the reusable
`picker::Provider` interface; `StaticProvider` implements the JSONL-backed
filter used by the command-line tool.

## ininspect

`ininspect` reads one versioned object record from stdin and renders its JSON
state as an expandable tree. Runtime input and result data stay on stdin and
stdout; interaction and rendering use `/dev/tty`.

```sh
printf '%s\n' \
  '{"schema_version":1,"object_id":"counter_123","class_name":"Counter","data":{"value":42,"options":{"enabled":true}}}' |
  ininspect --result-json
```

Normal viewing is presentation-only. Pressing `e` on a scalar opens an inline
JSON-value editor. Enter returns a `proposed` result containing the typed path,
old value, new value, and original data snapshot; `ininspect` never applies the
change itself. The caller must validate the snapshot and proposal before
mutation.

```text
Up/Down, j/k       Move through visible tree rows
Enter, Space       Expand or collapse a container
Right/Left, l/h    Expand or collapse a container
e                  Edit a scalar as JSON and return a proposal
q                  Close with a viewed outcome
Esc, Ctrl-C        Cancel
```

`viewed` and `proposed` return status 0. Cancellation returns status 130. All
outcomes are single schema-versioned JSON objects on stdout.

## indiff

`indiff` reads a unified diff from stdin and presents it without applying or
otherwise modifying anything. Accepting is only a structured decision for the
caller; the caller remains responsible for validating and applying the change.

```sh
result=$(git diff | indiff --result-json --title 'Review proposed change')
```

The result is one JSON object with schema version 1, an `accepted`, `rejected`,
or `cancelled` outcome, and zero-based `accepted_hunks` and `rejected_hunks`
arrays. Accepting at least one hunk returns status 0, rejecting every hunk
returns 3, and Ctrl-C or a termination signal returns 130.

```text
a, y, Enter       Accept current hunk and advance
r, n              Reject current hunk and advance
A / R              Accept / reject every remaining hunk
q, Esc             Reject every remaining hunk
Ctrl-C            Cancel
Up/Down, Ctrl-P/N Scroll one line
PageUp/PageDown   Scroll ten lines
```

## Development

Useful checks:

```sh
cargo fmt
cargo check --bins
cargo test --lib
cargo test --test terminal_contract  # Unix PTY contract (requires Expect)
cargo test --test picker_contract    # inpick PTY/result contract
cargo test --test inspector_contract # ininspect PTY/result contract
cargo test --test review_contract    # indiff PTY/result contract
cargo build --bins
```

## License

`navsplat` is licensed under GPL-3.0-only. See `LICENSE`.
