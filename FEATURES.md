# InfEditor Feature List and Progress

## Feature progress

| Feature | Progress |
|---|---|
| Selection rendering (grayed out when unfocused) | ✅ Done |
| Cursor (line/block/underline, blinking, adjustable width) | ✅ Done |
| Find / replace / go to line (plain/regex/escape, match case/whole word, F3) | ✅ Done |
| Incremental search (highlight & jump while typing) | ✅ Done |
| Native find/replace popup (direction group, match count, history dropdown) | ✅ Done |
| Context menu (edit / selection / bookmarks / comments groups) | ✅ Done |
| Docked find bar layout | ✅ Done |
| Button & menu animations (hover/pressed tri-state) | ✅ Done |
| Menu popup layering (root-level Overlay) | ✅ Done |
| Merged title/menu bar (frameless, drag/resize/double-click maximize) | ✅ Done |
| Title-bar right-click system menu / Alt mnemonics / always-on-top | ✅ Done |
| Menu items / settings items (all 12 top-level menus implemented) | ✅ Done |
| Unified shortcut table (single source of declaration) | ✅ Done |
| Vim mode (normal/insert/visual/command-line) | ✅ Done |
| Light-theme menu bar | ✅ Done |
| In-app popups (replacing rfd native dialogs) | ✅ Done |
| Preferences popup (category list + panels, native controls) | ✅ Done |
| File association + system context menu (HKCU registry) | ✅ Done |
| Owner-drawn scrollbars (drag/page/arrow, horizontal progressive scan) | ✅ Done |
| Code minimap (ink bar, find markers; auto-off past 4 MiB) | ✅ Done |
| JSONL/NDJSON preview (in-place expandable folding tree) | ✅ Done |
| Macros (record/stop/play/save/run) | ✅ Done |
| Settings persistence + recent files | ✅ Done |
| Run dialog (F5 + variables, recent runs) | ✅ Done |
| MD5 / SHA-1 digests | ✅ Done |
| JSON / XML formatting | ✅ Done |
| WYSIWYG Markdown document view (incl. chemistry formulas `\(...\)`/`\ce`) | ✅ Done |
| In-place source expansion: a selection spanning **two or more blocks** turns every block it touches into its Markdown (the unit is the block — a selection inside one block stays typeset however many lines it wraps to) — inline delimiters plus the block prefix (`#`, `-`/`1.`/`- [x]`, `> `, code fence, table `|` + delimiter row, `---`); a formula at the caret expands to its LaTeX source on its own and is editable character by character (a display `$$…$$` block is the exception: it keeps its typeset form at the caret — the whole block is the formula, so expanding it would leave the block permanently in source and break `Home`/`End`; sweep it to see its source), and a selection that stays inside one formula / image alt keeps it expanded (the highlighted text is the source, and Delete takes the covered characters instead of the whole node) | ✅ Done; all markers are virtual bytes (`data-vmark`) so caret offsets, undo and save are untouched; a table expands as one block but keeps per-cell `data-bid`; the expansion keeps every block's box (same cell padding, a slim delimiter row, measured `min-height` that is re-measured per document) and tints the expanded lines; it is live — a drag turns each block into source the moment the pointer reaches it (the widget owns the drag; blitz's native drag is switched off because its anchor is a node id) |
| CSV table view (RFC 4180) | ✅ Done |
| Non-standard math macro normalization (`\lt`/`\pmod` etc.) | ✅ Done |
| Markdown-escaped LaTeX compatibility (`x\_n`, `\\geq`, `\left{` render) | ✅ Done |
| Math highlighting in the Markdown source (`$…$`, `$$…$$` and `\[…\]` incl. multi-line blocks, `\(…\)` / `\\(…\\)`; commands / numbers / operators / braces / `%` comments / `\text{}` colored inside the formula; active in paragraphs, headings, block quotes, list items and table cells, and a display fence also works behind a `>` or list prefix) | ✅ Done; a block opener may carry the formula's first line (`$$ f(x) …`), an unterminated block runs to the end of the file like an unterminated fence, and inline math never crosses a line — a cross-line `\(…\)`, which the preview still typesets, stays uncolored rather than swallowing the file |
| Multi-click selection (double-click word / triple-click line) | ✅ Done |
| Column (rectangular) selection and editing (typing / cut / paste per column) | ✅ Done |
| Bracket matching (Ctrl+P to jump) | ✅ Done |
| Auto-close brackets/quotes | ✅ Done |
| Line bookmarks (Ctrl+F2/F2/Shift+F2) | ✅ Done |
| External modification detection (mtime comparison prompt) | ✅ Done |
| Ten UI languages (compile-time catalog, live switch; zh-CN, en-US, ja, ko, fr, de, es, pt, it, ar; Arabic strings ship, RTL layout pending) | ✅ Done; see `crates/i18n/README.md` |
| Word count / character code values | ✅ Done |
| Unicode normalization (NFC/NFD) | ✅ Done |
| Full-width/half-width conversion | ✅ Done |
| Insert numbering | ✅ Done |
| Window title sync (taskbar/Alt+Tab) | ✅ Done |
| Single instance per config directory (a later launch hands its files to the running window and exits; dropping a file on `infeditor.exe` opens it there) | ✅ Done |

## To do

| Feature | Progress |
|---|---|
| Line-wrap measurement optimization for very long paragraphs (WrapIndex Fenwick) | ⬜ Not started |
| Menu open slide-in animation | ⬜ Not started |
