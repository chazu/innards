# Changelog

## 2026-09-11

- The `inagent` composer soft-wraps long lines, reflows on resize, and scrolls
  to keep the cursor visible. Wrapping preserves Unicode graphemes and the
  original message text.
- Added `inagent`, an inline live conversation applet with a multiline composer,
  scrolling/search, explicit earlier-history requests, pause/resume/stop intents,
  and detach that preserves agent execution. JSONL stays on stdin/stdout; UI
  rendering and bracketed paste use the controlling terminal.
- Session updates preserve drafts and backlog position. Visible messages emit
  read intents; sends clear drafts only after acknowledgement. PTY tests cover
  live updates, resize, draft discard, protocol output, and terminal restoration.

## 2026-09-10

- Keep a space between candidate types and names even when the type is wider
  than its padded column, such as `instance_variable`.
- Added `C-x ^` / `C-x -` to grow/shrink every visible inline view by one row,
  with Alt-Down/Up aliases and shared terminal/layout bounds. Resizing retains
  search, selection, and edits, and uses the known cursor anchor without
  reading queued keystrokes as terminal-position responses.
- Added optional `inpick --preview-hook` notifications after readable previews
  appear. Callers can update state and return display metadata to refresh a row.
  Unseen or unreadable candidates do not notify; revisits notify only once.

## 2026-09-09

- Added mouse-wheel scrolling to inpage: three lines per tick when the pointer
  is over the pager. Mouse capture is released on normal exit, Ctrl-C, and
  handled termination signals.
- Added an optional `display` object to inpick records for compact presentation.
  These rows show a prefix and label, hide source paths and repeated metadata,
  and allocate more space to the preview. The preview title is customizable.
- Included compact prefixes and additional `search_text` in inpick filtering.
  Selection and Ctrl-D actions retain the complete original candidate record.
  Records without `display` keep the existing source-navigation layout.
- Added terminal and rendering regression coverage for mouse cleanup, compact
  message previews, search, and selection/result contracts.

## 2026-09-08

- Added `inpick --ctrl-d-action ACTION` to return a caller-selected action with
  the highlighted record. Callers such as Trashtalk can archive a message
  directly; inpick itself does not mutate the selected object.
