# Command palette — implementation plan

A Zed-style command palette for pane. `ctrl-shift-p` opens a centered modal, you
type, and a fuzzy-matched list of available commands appears with their key
bindings. Enter runs the selected command.

This follows Zed's conventions closely, because they are good and because the
project already lives in that lineage. The research behind this plan read Zed's
`command_palette`, `picker`, `fuzzy`, and `fuzzy_nucleo` crates at rev
`5e1fd39`.

## What we take, and what we build

Zed's palette is three crates deep: `command_palette` → `picker` → `ui`. We
depend on none of them. `picker` is a general, delegate-driven component with
resizable geometry, preview panes, multi-select, and sqlite persistence. We have
exactly one picker, so we write a concrete ~300-line type instead of a trait with
30 methods.

**Depend on:** `fuzzy_nucleo` (git, same Zed rev). Its dependency list is
`fuzzy`, `nucleo`, `gpui`, `gpui_util`, `path` — everything but `nucleo` is
already in our graph or trivial, and the licenses are compatible with ours.
It gives us two things worth not rewriting:

- A **synchronous** `match_strings`, which suits our scale. Zed matches ~2000
  actions on a background executor; pane will have well under 100, so matching
  runs on the foreground and the whole `Task` / pending-update / 16ms-block
  machinery in `picker` disappears.
- The **smart-case rule**, which Zed learned the hard way and documented in a
  six-line comment: match case-insensitively, then use case as a *scoring*
  signal (a `0.9^mismatches` penalty). Matching case-sensitively rejects
  `"Editor: Backspace"` against the action named `editor: backspace`, which
  breaks the palette's central use case.

**Copy verbatim** (pure `&str -> String`, no dependencies, take the tests too):

- `humanize_action_name` — `pane::SplitRight` → `"pane: split right"`, with
  acronyms preserved (`OpenURL` → `"open URL"`).
- `normalize_action_query` — collapses `::` → `:` and `_` → space, so typing a
  keymap-style query still matches the humanized name.

**Build ourselves** (~700 lines total):

| Piece | Size | Notes |
|---|---|---|
| `HighlightedLabel` equivalent | ~40 | Coalesce byte offsets into ranges, then `StyledText::with_default_highlights`. Highlight color is `colors().text_accent`. |
| Keycap renderer | ~80 | `KeybindingKeystroke` implements `Display`. Small bordered `div` per keystroke. |
| Row renderer | ~50 | We need about 10% of Zed's 550-line `ListItem`. |
| `CommandPalette` entity | ~300 | State, `uniform_list`, key handling, matching. |
| Modal container | ~40 | Reuses the conflict dialog's scrim shape. |
| Action enumeration and dispatch | ~150 | See below. |

## The three details that carry the Zed feel

**1. Capture the previous focus handle before the palette takes focus.**

This is load-bearing three separate times, and getting it wrong breaks the
feature in three different ways:

- `window.available_actions(cx)` walks the dispatch path from the *focused*
  element to the root. Call it after the palette focuses and you enumerate the
  palette's own actions.
- Key bindings are resolved against that handle
  (`highest_precedence_binding_for_action_in`), so the palette shows the binding
  that would actually fire in the pane you came from.
- On confirm, focus must return there **before** dispatch, or the action
  dispatches into the palette's dispatch path and nothing handles it.

The confirm order is exactly: `window.focus(&previous)` → dismiss → 
`window.dispatch_action(action, cx)`.

**2. Availability is not "all registered actions."**

`window.available_actions(cx)` returns actions that some element on the current
dispatch path registered an `on_action` handler for, plus globally-registered
ones. This is what makes the palette honest: with no selection, `Rename` does
not appear. `App::all_action_names` is the wrong call and Zed's own docs say so.

One gotcha: an action that cannot be built from `Default` or JSON is silently
dropped. If a pane action goes missing from the palette, that is why.

**3. Encode recency in the candidate id.**

Sort the command list by usage count before building match candidates, so
`candidate_id` *is* the MRU rank. The matcher's `Ord` already breaks score ties
on `candidate_id` ascending. Frequently-used commands therefore win ties at zero
cost, and an empty query shows the MRU order. Ten lines, and it is the thing
that makes a palette feel like it knows you.

Zed stores invocation counts in sqlite. We do not have a database, so counts go
in `~/.local/share/pane/command-history.json`, written from a background task on
confirm. In-memory-only would reset every launch, which makes the feature
close to pointless.

## Changes to existing code

`PathEditor` becomes the query input. It needs two additions:

- Emit an `Edited` event on every text change, so the palette re-matches per
  keystroke. It currently emits only `Committed` and `Cancelled`.
- Optional placeholder text, drawn in `text_muted` when the content is empty.

Zed reaches its editor through an `ErasedEditor` trait behind a `OnceLock`
factory. That indirection exists only to break a crate cycle in their workspace.
We call `PathEditor` directly.

`Workspace` gains the palette in an `Option<Entity<CommandPalette>>`, rendered in
the same scrim used by the conflict dialog.

## Behavior

- `ctrl-shift-p` opens; pressing it again closes. `escape` or a click outside
  also closes.
- Up and down move the selection and **wrap around**. Hovering a row selects it,
  which is a large part of why Zed's palette feels responsive.
- Matched characters are highlighted in the accent color. Byte offsets, not
  character indices — this is a debug panic in Zed's label and would be a
  rendering bug in ours.
- With no matches, a disabled "No matches" row appears in place of the list, so
  the modal does not collapse and jump.
- No result cap and no debounce. At our action count both are unnecessary
  complexity.
- Width 34rem, max height 24rem, positioned 5rem from the top and centered.
  Height is a maximum: a three-result palette is short.

## Sequencing

1. `PathEditor` gains the `Edited` event and placeholder support. Verify with a
   throwaway view before building on it.
2. The palette entity: enumeration, humanizing, matching, list rendering,
   selection, confirm. No key bindings shown yet.
3. Keycap rendering and the modal container.
4. MRU persistence.
5. Audit action names. `humanize_action_name` only reads well if the actions are
   named well, and ours were written for keymaps, not for display. Some will
   want renaming, and a few internal ones will want hiding — a
   `HIDDEN_NAMESPACES` set is a `HashSet<&'static str>`, not the
   `command_palette_hooks` crate.

## Deliberately deferred

- **A place finder.** Zed splits commands (`ctrl-shift-p`) from files
  (`ctrl-p`), and that split is right: mixing directories into the command list
  makes both harder to scan. Once this palette exists, a `ctrl-p` finder over
  home, bookmarks, mounted volumes, and recent directories reuses everything
  here except the entry source. That is the piece that removes the need for a
  sidebar.
- Query history (shell-style up-arrow through past queries).
- A `:`-style command interceptor for vim-like commands.
- Resizable or persisted modal geometry.
