# Innards UI performance and correctness receipts — 2026-09-26

These measurements cover the initial retained toolkit in the local Trashtalk
and Innards checkouts on macOS. They are host-specific evidence, not universal
latency guarantees or human UX acceptance. The workload is synthetic. The
full Trashtalk suite was running during the final measurements, so differences
of a few microseconds/milliseconds do not establish an instrumentation cost.

## List/detail gate

The gate was exercised before the additional controls and plot were added:
10,000 records, visible rows only, local selection and detail changes, complete
row count and endpoint checks, keyed replacement, and no outgoing model request
for 200 cached navigation/redraw cycles. A release run of 1,000 interactions
had input plus test-buffer rendering p99 of 126 µs with profiling disabled and
104 µs enabled. Single-row replacement p99 was at most 1 µs at the timer's
resolution. These are **in-memory Ratatui buffer** timings, not terminal writes.

The final implementation was measured again after adding controls, protocol
checks, request receipts, and byte limits:

| Native release path, 1,000 samples | Profile off | Profile on |
| --- | ---: | ---: |
| Input + test-buffer render p50 | 107 µs | 107 µs |
| Input + test-buffer render p99 | 171 µs | 167 µs |
| Input + test-buffer render max | 345 µs | 287 µs |
| Single-row update p99 | 6 µs | 5 µs |
| Single-row update max | 33 µs | 25 µs |

The same row count and endpoint content assertions pass in both modes. Empty
profiling maps/receipts are tested when profiling is disabled. The paired result
shows no measurable slowdown at this sample size; it is not proof of zero cost.
[Raw native receipt](benchmarks/2026-09-26-ui-native.jsonl).

## Bash and transport

`bin/trash-bench-ui` sends a real 10,000-row initial frame through the public
handler/duplex bridge, requests a 16-row append, checks the batch and its ack,
and contrasts 100 with 10,000 initial rows.

| Observer mode | 100 rows | 10,000 rows |
| --- | ---: | ---: |
| Profile off, action round trip | 52.7 ms | 36.5 ms |
| Profile on, action round trip | 31.3 ms | 41.5 ms |
| DEBUG process tracing, action round trip | 42.3 ms | 44.9 ms |
| jq launches, profile off | 7 | 7 |
| jq launches, profile on | 8 | 8 |
| SQLite launches | 1 | 1 |
| Bash processes, DEBUG observer | 24 | 24 |

Counts cover startup, one action, and detach. The additional profiled jq call
writes the summary. Startup accounts for the SQLite invocation. These are
bounded independently of collection size; they are not one invocation per row
or cell. Each run observes one action, so the round trips are samples rather
than percentile estimates. Earlier gate samples were roughly 26–30 ms.

PATH wrappers are present in all these bridge runs and add observer overhead.
DEBUG process tracing is present only in the labeled mode. Neither mechanism
claims to be a complete OS exec/database census. The initial read timer starts
in the child after the handler has prepared its snapshot; it must not be read
as complete application startup time.
[Raw bridge receipt](benchmarks/2026-09-26-ui-bridge.jsonl).

## Live terminal and overload evidence

The automated live-terminal test uses 10,000 rows and a deliberately two-second
handler delay. Typing and redraw remain available during that delay; local
navigation sends no requests; exactly one action contains the committed draft;
a later edit survives its acknowledgement. It checks terminal restoration and
that stdout contains protocol records rather than escape sequences.

A separate live check exercised the complete Trashtalk → `inui` path with the
event feed, plot, a debounced filter, profile output and detach, plus three panes
of compositional inspection. In this **debug build**, three sampled local
interactions completed terminal output within 2.52 ms. Six draws, including the
initial plot, had a 14.35 ms maximum. Initial 1.55 MB JSON decode took 45.04 ms;
this is startup work and demonstrates why large snapshots are not the update
path. The filter request round trip was 39.52 ms. Native input and asynchronous
model response are reported separately.
[Native live summary](benchmarks/2026-09-26-ui-live-native-debug.json),
[Bash live summary](benchmarks/2026-09-26-ui-live-bash.json).

The installed **release binary** was then checked through the same full path.
Its three local interaction samples completed terminal writes within **0.85 ms**;
six draws, including the initial plot, had a **3.04 ms** maximum. Initial decode
was 6.85 ms. The asynchronous filter round trip was 60.16 ms and its resulting
frame reached the terminal at 69.85 ms, separately from local editing.
[Release native summary](benchmarks/2026-09-26-ui-live-native-release.json),
[release Bash summary](benchmarks/2026-09-26-ui-live-bash-release.json).

Installed artifact: `~/.cargo/bin/inui`, SHA-256
`e05db30a91ee43f31b97d1af56281ec0a7ee9bed97110b9b7d1e75a12494c5d7`,
matching the checkout's `target/release/inui`.

Regression tests also cover:

- Blocked stdout with bounded queues and immediate terminal-thread return.
- Oversized/unterminated input bounded before JSON parsing; oversized row
  replacements rejected without mutation.
- Atomic invalid batches and deduplicated resync requests.
- Stale window/query generations, cache eviction and stable order validation.
- Retained selection after removal, ring retention and exact row content.
- Keyed local drafts, form value gathering and native split interaction.
- A single-sample spike surviving 10,000-point plot decimation.
- Duplicate bridge requests without re-executing the domain action.
- Ordinary block dependency capture and read-only signal enforcement.
- Inspector branch replacement and responsive parent-pane hiding.

The full Innards suite passes serialized. The initial parallel run hit existing
PTY fixture collisions (`AlreadyExists` and cursor-query contention); serialized
reruns pass without changing those applets. A manual user playtest and a
production-domain adapter remain separate from these automated/synthetic checks.

## Final validation

- Trashtalk `LC_ALL=C TRASH_TEST_JOBS=2 TRASH_TEST_TIMEOUT=300 make verify`:
  102 runtime files and 50 compiler files passed, no failures/timeouts.
- Innards `cargo test --all-targets -- --test-threads=1`: all targets passed;
  the ignored release benchmark was run explicitly. Subsequent focus/profile
  refinements passed the complete UI unit group and live-terminal contract.
- Complete installed-release feed/filter/plot and three-pane inspector checks
  passed. Bash source checks and both repository diff whitespace checks passed.
