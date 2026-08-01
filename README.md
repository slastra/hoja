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
- **File transfer.** pane selects the fastest correct method for each file:
  - A move on one filesystem uses `rename()`. This is instant.
  - A copy on btrfs or XFS uses reflink. This is instant for all file sizes.
  - All other copies use `copy_file_range` with a fallback. Sparse files stay
    sparse.
  - pane keeps permissions, times, extended attributes, symlinks, and
    hardlinks. Writes are atomic. On removable media, pane flushes data before
    it reports success.
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

| Key | Function |
|---|---|
| `ctrl-k ctrl-←/→/↑/↓` | Split the active pane in that direction |
| `ctrl-k ←/→/↑/↓` | Move focus to the adjacent pane |
| `ctrl-k ctrl-w` | Close the active pane |
| `alt-←` / `alt-→` | Go back / go forward in the history |
| `alt-↑` / `backspace` | Go to the parent directory |
| `alt-home` | Go to the home directory |
| `ctrl-l` | Edit the path |
| `enter` | Open the selected directory |
| `ctrl-c` / `ctrl-x` / `ctrl-v` | Copy / cut / paste |
| `ctrl-a` | Select all |
| `escape` | Clear the selection |

Mouse controls:

- Click a row to select it. Control-click to add or remove a row.
  Shift-click to select a range.
- Double-click a directory to open it.
- Right-click for the context menu: Open, Open With, Cut, Copy, Paste, and
  New Folder.
- Click a column header to sort. Click again to reverse the order.
- Drag the line between column headers to resize a column.
- The extra mouse buttons go back and forward in the history.

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
- A job journal for undo, trash, and resume after a crash
- Tabs, search, previews, and thumbnails
- Delta sync between machines

See `pane-transfer-plan.md` for the full design.

## License

The license of pane is GPL-3.0-or-later. See the `LICENSE` file.

GPUI has the Apache-2.0 license. The Zed `theme` and `file_icons` crates have
the GPL-3.0-or-later license. This is why pane uses the GPL.

The included assets have their own licenses:

- The icons come from Lucide and Zed. The license is ISC. See
  `assets/icons/LICENSES`.
- The Rosé Pine themes have the MIT license. See
  `assets/themes/rose-pine/LICENSE`.
