# Changelog

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
