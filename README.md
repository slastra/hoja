# pane

pane is a file manager for Linux. It runs on Wayland. It uses
[GPUI](https://www.gpui.rs/), the GPU-accelerated UI framework from the Zed
editor.

**Warning:** pane is experimental software. Do not use pane as your only tool
for important data.

![pane with two panes, Rosé Pine theme](docs/screenshot.png)

## Features

- **Panes.** Split the window into panes. Each pane shows one directory. The
  split tree is recursive, in the same shape as the Zed editor.
- **Fast lists.** A pane shows a directory of 100,000 files in less than one
  second. The list stays smooth when you scroll.
- **Navigation.** Each pane has its own history. Use the back, forward, up, and
  home buttons. Click the path, then type a new path.
- **View settings.** Each pane has a menu at the right end of the toolbar.
  It controls hidden files, folder grouping, and the sort order. pane does
  not show hidden files by default.
- **File transfer.** pane selects the fastest correct method for each file:
  - A move on one filesystem uses `rename()`. This is instant.
  - A copy on btrfs or XFS uses reflink. This is instant for all file sizes.
  - All other copies use `copy_file_range` with a fallback. Sparse files stay
    sparse.
  - pane keeps permissions, times, extended attributes, symlinks, and
    hardlinks. Writes are atomic. On removable media, pane flushes data before
    it reports success.
- **Delete with undo.** `delete` removes the selection and `ctrl-z` puts it
  back. pane moves the files to the trash directory of the freedesktop
  specification, on the same filesystem, so a delete is instant and other
  file managers can empty what pane deletes. pane has no other trash
  controls: no browser, no restore list, no empty command.
- **Clipboard.** Copy and paste files between pane and other file managers.
  pane reads and writes the GNOME clipboard format.
- **Themes.** pane reads Zed theme files. Put a theme file in
  `~/.config/pane/themes/`. pane applies changes to these files immediately.
  The Rosé Pine themes are included.
- **Icons.** File icons follow the Zed icon system. The theme sets the icon
  color.

## Build

1. Install Rust 1.95 or later. The Zed source sets this minimum.
2. On Arch Linux, install these packages:
   `clang cmake pkgconf fontconfig freetype2 wayland wayland-protocols
   libxkbcommon libxkbcommon-x11 vulkan-icd-loader alsa-lib openssl zstd`
3. Run `cargo build --release`.

Note: Cargo compiles GPUI with your local toolchain. The toolchain file in the
Zed repository does not apply to git dependencies.

## Start

```sh
pane [DIRECTORY] [--theme NAME] [--list-themes]
```

The `PANE_THEME` environment variable also sets the theme.

## Keys

Keys work on the pane that has focus.

**Move**

| Key | Function |
|---|---|
| `↑` `↓` | Move the selection one row |
| `pageup` `pagedown` | Move the selection one screen |
| `home` `end` | Move to the first or the last entry |
| `enter` | Open the selected entry |
| type a name | Jump to the first match. Repeat one letter to cycle the matches. |
| `alt-←` `alt-→` | Go back and forward in the history |
| `alt-↑` or `backspace` | Go to the parent directory |
| `alt-home` | Go to the home directory |
| `ctrl-l` | Edit the path |

**Select**

An outline marks the entry that the keys act on. It is usually also selected.
Use `ctrl` with the movement keys to move the outline alone, and `ctrl-space`
to add or remove that one entry. This builds a selection of entries that are
not next to each other.

| Key | Function |
|---|---|
| `shift-↑` `shift-↓` | Extend the selection one row |
| `shift-pageup` `shift-pagedown` | Extend the selection one screen |
| `shift-home` `shift-end` | Extend the selection to the first or the last entry |
| `ctrl-↑` `ctrl-↓` | Move the outline and keep the selection |
| `ctrl-space` | Add or remove the outlined entry |
| `ctrl-a` | Select all |
| `escape` | Clear the selection |

**Change files**

| Key | Function |
|---|---|
| `f2` | Rename the selected entry |
| `delete` | Delete the selection |
| `ctrl-z` | Undo the last delete |
| `ctrl-c` `ctrl-x` `ctrl-v` | Copy, cut, paste |

**Panes and view**

| Key | Function |
|---|---|
| `ctrl-k ctrl-←/→/↑/↓` | Split the active pane in that direction |
| `ctrl-k ←/→/↑/↓` | Move focus to the adjacent pane |
| `ctrl-k ctrl-w` | Close the active pane |
| `ctrl-h` | Show or hide hidden files |
| `ctrl-shift-d` | Dismiss finished transfers |

**Mouse**

| Action | Function |
|---|---|
| Click | Select the row |
| `ctrl`-click | Add or remove one row |
| `shift`-click | Select a range |
| Double-click | Open the entry |
| Right-click | Open the context menu |
| Click a column header | Sort. Click again to reverse the order. |
| Drag a header divider | Resize the column |
| Back and forward buttons | Go back and forward in the history |

**While you edit a path**

| Key | Function |
|---|---|
| `←` `→` or `ctrl-←` `ctrl-→` | Move one character or one word |
| `home` `end` | Move to the start or the end |
| add `shift` | Extend the selection instead |
| `ctrl-backspace` `ctrl-delete` | Delete one word |
| `enter` or `escape` | Go to the path, or cancel |

**In a menu or a dialog**

| Key | Function |
|---|---|
| `↑` `↓` | Move the highlight |
| `enter` or `escape` | Choose, or close |

## Transfer engine

The `pane-transfer` crate contains the transfer engine. It is a standard
Rust library with no UI dependencies. One worker thread does each job. The UI
reads progress from atomic counters.

To run the engine tests:

```sh
cargo test -p pane-transfer
```

The reflink test needs a btrfs filesystem. To prepare one:

```sh
./scripts/btrfs-loop.sh up
PANE_TEST_BTRFS=/tmp/pane-btrfs/mnt cargo test -p pane-transfer -- --ignored
```

## Roadmap

These features are planned and not complete:

- Parallel copy for many small files
- A job journal, for undo of a transfer and resume after a crash
- Drag and drop, between panes and with other applications
- A relative time option for the Modified column, such as "2 hours ago"
- Optional columns for the owner, the group, and the permissions
- Tabs, search, previews, and thumbnails
- Delta sync between machines

See [`docs/transfer-plan.md`](docs/transfer-plan.md) for the full design.

## License

The license of pane is GPL-3.0-or-later. See the `LICENSE` file.

GPUI has the Apache-2.0 license. The Zed `theme` and `file_icons` crates have
the GPL-3.0-or-later license. This is why pane uses the GPL.

The included assets have their own licenses:

- The icons come from Lucide and Zed. The license is ISC. See
  `assets/icons/LICENSES`.
- The Rosé Pine themes have the MIT license. See
  `assets/themes/rose-pine/LICENSE`.
