# Performance first tranche, Innards: I1 idle redraw and I3 picker caching

**Scope:** items I1 and I3 of `~/.trashtalk/docs/agent-performance-audit-2026-09-14.md`
(first-tranche item 5). Nothing here touches Trashtalk, Whisker, the
`inagent` snapshot protocol, the result JSON schemas, or dependencies.

**Status of the evidence.** The prior authoring session could not execute
`cargo`, so its original claims were deliberately labelled **inferred**. This
completion session ran the complete verification matrix on 2026-09-15. The
code-derived before/after rates remain **inferred** because no baseline binary
was run, but the deterministic proxies are now **measured**: the redraw state
machine drew its initial frame and no idle frame (2/2 tests), the preview cache
served repeated lookups without a second disk read (5/5 tests), the picker
rendered three redraws from one source read (1/1 test), and a long picker list
built only visible rows (1/1 test). Section 4 records every command and
result. Direct PTY CPU and tty-byte measurements remain intentionally
unmeasured, with the reproducible procedure and limitations in section 6.

## 1. Summary of the change

| Applet | Before (inferred from code) | After (inferred from code) |
| --- | --- | --- |
| `inpage`, `inmacs` | one full frame (layout + syntect highlight of the visible lines) every 80 ms poll timeout: 12.5 frames/s idle | zero frames while idle; a frame after each input event; the loop wakes every 250 ms only to check the termination flag |
| `inpick` | 12.5 frames/s idle, each frame reads and splits the preview file and builds a `ListItem` per match | zero idle frames; preview file read once per (path, size, mtime); only the visible rows are built |
| `ininspect`, `indiff` | 12.5 frames/s idle | zero idle frames |
| `inagent` | 12.5 frames/s idle plus the `viewed_intent` scan per frame | frames only on bridge input, terminal input, or the disconnect transition; poll 80 ms for 2 s after any activity, then 250 ms |
| `navsplat` | 25 frames/s idle (40 ms poll), each frame reading the preview file | frames only on LSP events, sent requests, input, or spinner ticks; 40 ms cadence only while a spinner is visible; otherwise 250 ms, or the remaining debounce if a query is pending |

The design follows the audit's I1 proposal: a pending-frame flag set by
every event source, drawn at most once per loop iteration, and a longer poll
when nothing animates. Input latency is unchanged because
`crossterm::event::poll` returns as soon as terminal input arrives; the
timeout only bounds how long a termination signal or a non-terminal event
source (bridge frames, LSP replies) waits.

## 2. Changes by file

### `src/redraw.rs` (new)

`Redraw` holds the pending-frame flag (`request`, `take`) and an activity
window (`activity`, `poll_timeout`) used by `inagent`, whose bridge frames
arrive on an `mpsc` channel that cannot wake the terminal poll. Constants:
`IDLE_POLL` 250 ms, `ACTIVE_POLL` 80 ms, `ACTIVE_WINDOW` 2 s. Two unit tests
cover "the first frame is due, idle ticks are not" and the activity window.

### `src/preview.rs` (new)

`PreviewCache` keeps the split lines of up to 16 recently previewed files,
keyed by path and validated on each lookup by `(len, mtime)`. A lookup costs
one `stat`; a changed or new file costs one read. Unreadable files are not
cached so a later lookup retries. `reads()` exposes the read count as the
deterministic proxy for tests. Five unit tests cover cache hits, same-length
rewrites detected through mtime, length changes, unreadable and empty files,
and eviction at capacity.

### `src/inline_text.rs` (`inpage`, `inmacs`)

`run_editor` no longer draws after a poll timeout. The first frame is drawn
by `run_with` before the loop; each later frame follows a key, mouse, resize,
or input-error event. Poll timeout 80 ms → `IDLE_POLL`.

### `src/inspector.rs` (`ininspect`), `src/review.rs` (`indiff`)

`Redraw` gates the draw; every event read requests a frame (including
non-key events such as resize, which ratatui's `autoresize` needs a draw to
apply). Poll timeout 80 ms → `IDLE_POLL`.

### `src/picker.rs` (`inpick`)

- Loop: `Redraw` gates the draw; every event requests a frame. When the
  preview hook returns a display object, `notify_preview` now returns `true`
  and the loop draws again immediately (the row update used to appear on the
  next 80 ms tick).
- `StaticProvider::new` precomputes one lower-cased haystack per candidate in
  the same field order the per-keystroke `format!` used. `search` is now
  `contains` per term per candidate with no allocation besides the result
  clones. `search_haystack` is unit-tested for exact output so the matching
  behaviour is demonstrably unchanged.
- `App` caches the property-column union (`columns`) and recomputes it when
  the matches change or the preview hook updates a display, instead of
  scanning every match per frame.
- `draw_candidates` builds only the rows that fit. A fresh `ListState` /
  `TableState` (which the previous code created every frame) scrolls so the
  selection sits on the bottom row once it passes the visible height;
  `visible_window_start` reproduces that placement exactly, so the rendered
  window is unchanged. Unit tests cover the function and a rendered 40-row
  list with the selection at row 30.
- `draw_preview` uses `PreviewCache`; a rendering test asserts one file read
  across three frames.

### `src/bin/navsplat.rs`, `src/bin/navsplat/ui.rs`

- Loop: frames are requested by `drain_lsp_events`, `maybe_send_query`, and
  `maybe_request_side_pane` (each now returns whether it changed state), by
  input events, and by spinner ticks. `spinner_visible` mirrors the two
  spinner conditions in `ui`; while one is visible the loop keeps the 40 ms
  cadence and advances `tick`, so the animation looks as before.
  `idle_poll_timeout` waits `IDLE_POLL`, shortened to the remaining debounce
  when a query is pending, and falls back to `IDLE_POLL` when the debounce
  has expired without a send (query equal to the last one) so the loop cannot
  spin.
- `draw_preview` resolves a `PreviewTarget` (side-pane hit or active symbol,
  same precedence and highlight range as before) and reads through the
  `PreviewCache` stored on `App`.

### `src/lib.rs`

Registers the two new modules.

## 3. Evidence, inferred

Per idle second with no input and no bridge/LSP traffic:

| Quantity | Before | After |
| --- | --- | --- |
| Frames drawn, `inpage`/`inmacs`/`inpick`/`ininspect`/`indiff`/`inagent` | 12.5 | 0 |
| Frames drawn, `navsplat` (idle, spinner hidden) | 25 | 0 |
| Loop wake-ups (poll timeouts), all applets | 12.5 (25 for `navsplat`) | 4 |
| Preview file reads, `inpick` | 12.5 | 0 |
| Preview file reads, `navsplat` | 25 | 0 |
| `viewed_intent` visible-set scans, `inagent` | 12.5 | 0 |

Per event:

| Quantity | Before | After |
| --- | --- | --- |
| `inpick` preview reads per redraw of an already shown file | 1 read + split | 1 `stat` |
| `inpick` `ListItem`s built per frame | every match | at most the list height |
| `inpick` per-keystroke work per candidate | `format!` of 8 fields + `to_lowercase` + `contains` | `contains` |
| `inpick` property-column union scans | 1 per frame | 1 per match change |

Each frame that no longer happens also removes the work ratatui does per
`Terminal::draw` regardless of diff results: a terminal size query
(`autoresize`) and the cursor show/hide and position sequences written to the
tty. Whether ratatui 0.30 guards those writes could not be checked here
(dependency sources were outside the sandbox); treat the "bytes written while
idle" proxy in section 6 as the way to find out.

Latency changes (inferred, all bounded by the new poll timeouts):

- Keystroke to redraw: unchanged (the poll wakes on input).
- `inagent` bridge snapshot to redraw: ≤80 ms while active (any event in the
  last 2 s, which covers 1 Hz streaming), ≤250 ms after idle. Previously ≤80 ms.
- `navsplat` LSP reply to redraw: ≤40 ms while a spinner is visible (a request
  in flight always sets `loading`), ≤250 ms otherwise (only late progress
  notifications).
- Termination signal to exit: ≤250 ms idle (previously ≤80 ms). The PTY
  contract tests allow 10 s.

## 4. Verification and deterministic proxies

The following commands ran successfully in `/Users/chazu/dev/rust/innards` on
2026-09-15:

```sh
cargo fmt --all                         # passed
cargo clippy --all-targets               # passed (pre-existing warnings only)
cargo test --lib                         # 81 passed, 0 failed
cargo test --bins                        # passed
cargo test --test terminal_contract      # 7 passed, 0 failed
cargo test --test picker_contract        # 5 passed, 0 failed
cargo test --test inspector_contract     # 2 passed, 0 failed
cargo test --test review_contract        # 2 passed, 0 failed
cargo test --test resize_contract        # 5 passed, 0 failed
cargo test --test agent_contract         # 2 passed, 0 failed
```

The scoped deterministic proxy commands also passed:

```sh
cargo test --lib redraw::tests
# 2 passed: initial draw is due, idle `take()` does not draw, activity expiry restores idle polling
cargo test --lib preview::tests
# 5 passed: cache hit, same-length mtime invalidation, missing/empty files, and capacity eviction
cargo test --lib picker::tests::preview_source_is_read_once_across_redraws
# 1 passed: three picker redraws read the preview source once
cargo test --lib picker::tests::long_candidate_lists_build_only_the_visible_rows
# 1 passed: a 40-row picker list builds only the visible window
```

These tests measure exact internal outcomes rather than elapsed CPU time:
`Redraw::take()` is false after an idle timeout, and `PreviewCache::reads()`
does not increase on a cache hit. They are deterministic and portable, but do
not measure scheduler wakeups, ratatui terminal bytes, or process CPU.

`clippy` also reports existing warnings outside this tranche in
`tests/terminal_contract.rs` and `src/bin/navsplat/clipboard.rs`; it exits
successfully and reports no warning from the new I1/I3 code. The scoped
`navsplat.rs` collapsible-`if` warning was corrected before this final run.

## 5. Caveats

- **No direct idle CPU or PTY-byte baseline was collected.** The deterministic
  tests prove the gating and cache behavior, and the PTY procedure below is
  ready for a real-terminal before/after measurement. The stated rates remain
  code-derived rather than profiler measurements.
- `PreviewCache` trusts `(len, mtime)`. A rewrite within the filesystem's
  mtime granularity that keeps the length is served stale until the next
  change. APFS has nanosecond mtimes; the picker's previews are message files
  and source files, so this is acceptable for the first tranche.
- `inagent` still polls: frames from the bridge cannot wake
  `crossterm::event::poll`. The activity window keeps streaming latency at
  the old bound; the first snapshot after a quiet period can take up to 250 ms
  to appear.
- `navsplat` keeps the 40 ms cadence whenever `loading` is true. An existing
  quirk keeps `loading` set when a query is retyped to equal the last sent
  one; that state now costs the same as before (25 frames/s), not less.
- Draws that change nothing still happen once per input event (for example an
  unbound key). ratatui diffs the buffers, so that costs a layout pass, not
  terminal traffic.
- Terminal resize is applied on the next draw. Every event read requests a
  frame, so a resize event triggers one; a resize that arrives while idle is
  drawn at once because `crossterm` reports it as an event.

## 6. Measuring on a real terminal

Run from an ordinary shell after `cargo build --release --bins`; the
authoring sandbox could not. Two proxies per applet, before and after (build
the baseline from commit `3b5b64d` into another target directory):

1. **CPU time idle for 60 s** with `/usr/bin/time -p` inside a pty.
2. **Bytes written to the tty while idle** for 10 s, which counts frames even
   when ratatui's diff is empty (cursor sequences per draw).

```python
import os, pty, select, subprocess, sys, time
binary, quit_keys = sys.argv[1], sys.argv[2].encode().decode('unicode_escape').encode()
args = sys.argv[3:]
master, slave = pty.openpty()
proc = subprocess.Popen(['/usr/bin/time', '-p', binary, *args], stdin=slave,
                        stdout=slave, stderr=slave, preexec_fn=os.setsid)
os.close(slave)
def pump(seconds, reply_cpr):
    total, deadline = 0, time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([master], [], [], 0.05)
        if ready:
            data = os.read(master, 65536)
            total += len(data)
            if reply_cpr and b'\x1b[6n' in data:
                os.write(master, b'\x1b[1;1R')
    return total
pump(2, True)                       # startup and cursor-position handshake
idle = pump(10, False)              # bytes written while nothing happens
time.sleep(50)                      # rest of the 60 s idle window
os.write(master, quit_keys)
tail = b''
while proc.poll() is None:
    ready, _, _ = select.select([master], [], [], 0.2)
    if ready:
        try: tail += os.read(master, 65536)
        except OSError: break
print(f'idle bytes over 10 s: {idle}')
print(tail.decode(errors='replace').strip().splitlines()[-3:])   # real/user/sys
```

Examples (quit keys as escapes):

```sh
python3 idle.py target/release/inpage 'q' README.md
python3 idle.py target/release/inmacs '\x18\x03' README.md          # C-x C-c
python3 idle.py target/release/ininspect 'q' --result-json < object.json
python3 idle.py target/release/indiff 'q' --result-json < change.diff
python3 idle.py target/release/inpick '\x1b' --result-json < candidates.jsonl
```

For `inagent`, feed one snapshot on stdin and quit with `\x18\x03`; for
`navsplat`, run inside a Rust workspace with `rust-analyzer` on `PATH`, wait
for "match(es)" before starting the idle window, and quit with `\x1b`.

Expected if the inference holds: user+sys CPU over 60 s drops from a few
hundred milliseconds to a few milliseconds, and idle bytes drop to 0.

## 7. Follow-ups

- Give `inagent` a real wake source: forward terminal events from a reader
  thread into the same channel as bridge frames and block on `recv_timeout`,
  which removes the activity window and the 250 ms first-frame latency. Not
  done here because it changes the input path and the audit's I2/T5 delta
  protocol will touch the same loop.
- I2 (row rebuild keyed by entry id; `unicode_width` instead of
  `Span::raw(ch.to_string()).width()` in `agent::wrap`) is the next Innards
  item; the frame gating here makes its per-snapshot cost the only remaining
  idle cost of an attached view.
- I4 (packed syntect syntax set) and I5 (grouped spans in
  `inline_text/render.rs`) remain as in the audit.
- `navsplat` could stop keeping `loading` set when a retyped query equals the
  last request; then its idle cost after such an edit would also be zero.
- Consider a `INNARDS_DRAW_STATS` style counter printed at exit if the PTY
  byte proxy proves noisy; `Redraw::take` is the single place to count.
