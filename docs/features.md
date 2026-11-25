# Features

What each feature does, in full. The README has the short list.

## Panes and tabs

### Recursive pane layout

Split the window into panes. Each pane shows one directory, and panes can be
split recursively into any layout.

### Tabs

Each pane can hold tabs, as Zed's panes do, and each tab has its own folder
and history. The tab bar shows when a pane has a second tab, or when the
window has more than one pane. Drag a tab to another pane, or to the edge of a
pane to split a new pane off there, as in Zed. Middle-click a folder to open
it in a tab, and drop files on a tab to put them in its folder. A tab that is
not shown stops watching its folder and reads it again when you show it.

`hoja PATH...` opens each path in a tab, in order, and shows the first. A file
opens its folder with the file selected. A path that is not there is an error,
and no window opens.

### Fast directory listings

Rows are virtualized, so even very large directories stay smooth while
scrolling.

### Independent navigation

Every pane has its own history. Go back, forward, up, or home, or click the
path to jump to another location.

### Recursive folder sizes

Folder sizes are calculated in the background and appear only when complete,
so values never count upward in the listing. Sorting by size happens once,
after every folder has a final value.

### Pane footer

Each pane summarizes exactly what it contains: total items and size, selected
files, permissions and ownership for a single file, or live search progress.

### Clear active pane

The active pane keeps full emphasis while inactive panes dim their names,
icons, paths, and selections. Git status colors are preserved.

### Per-pane view settings

Hidden files, folder grouping, and sort order are configured independently for
each pane. Hidden files are off by default.

### Live listings

Hoja watches every directory it displays. If another program changes the
contents, the pane refreshes without losing your selection. If the directory
disappears, Hoja moves to the nearest existing parent.

## Viewing files

### List or grid

A pane shows its entries as a list with columns, or as a grid of cards. Switch
with the two buttons in the toolbar, with `ctrl-1` and `ctrl-2`, or in the
view menu. In the grid, a file with a picture shows it at the shape of the
picture, and other files show their name. The cards keep the sort order from
left to right across each band, so the arrow keys move as they read. In the
grid, the view menu's Columns list becomes Card Details, and sets which
details a card shows under its name. `ctrl-+` and `ctrl--`, or `ctrl` with the
scroll wheel, make the cards larger or smaller, and `ctrl-0` puts the default
size back.

### Hover previews

Rest the pointer on a file to see it without opening it. An image or a video
shows a picture, its size and, for a video, its length. A PDF shows its first
page and its page count. An audio file shows its tags and length, and a text
file shows its first lines, with syntax colors as in Zed for the languages Zed
itself bundles: Rust, Python, TypeScript and JavaScript, Go, C and C++, shell,
CSS, JSON, YAML, Markdown, and diffs. A file of another type shows a picture
when a thumbnailer for its type is installed. The pictures come from the
thumbnail cache that other file managers share. Hoja makes a picture only when
the cache does not have one, and while it does, the preview keeps a place of
the correct shape for the picture. Archive members and folders show no
preview.

### Previews for more types

Hoja has no plugins of its own for previews. It uses freedesktop thumbnailers:
a program, and a `.thumbnailer` file in `/usr/share/thumbnailers` or
`~/.local/share/thumbnailers` that names the types it takes. The usual desktop
packages add fonts, office documents and comics. Install one, and the next
Hoja to start shows pictures of those types. `hoja-model-thumbnailer`, in this
repository, is one: it draws STL and 3MF models, and it shows the picture that
a slicer puts in a 3MF or a G-code file. G-code with no picture shows its
text. Other file managers use it too.

### Archives as folders

Browse `.zip`, `.tar`, `.tar.gz`, `.tar.bz2`, `.tar.xz`, and `.tar.zst`
archives like directories. Tar archives stream while loading so files appear
immediately, and archive folders report their recursive sizes instantly. Copy
files out using the keyboard, context menu, or drag and drop. Archives are
always read-only.

## File picker

### For other applications

With `hoja-portal` installed and named in the portal's config, an
application that asks the desktop portal for a file, or for where to save
one, gets Hoja. The README
says how to set it up. Each request starts a Hoja of its own, which answers
and exits.

The picker is all of Hoja: panes, tabs, search, previews and file operations
work as usual. A footer, as Zed's pickers have, shows the filter and what
the accept button would return on the left. On the right are Open Folder,
Cancel, and the accept button with the label the application gave it, each
with its key.

### What it picks

- To pick one file, select it and press the accept button, or press enter or
  double-click on it. A click with ctrl or shift selects one file too.
- To pick several files, select them, then press the accept button or enter.
- To pick a folder, go into it and press the accept button, or select one
  folder in it first. A folder picker shows no files.
- When the selection is one folder, the accept button goes into it, as in the
  GTK dialog.
- To pick a folder when the application asked for files, press Open Folder
  or ctrl-enter. It picks the selected folders, or the folder you are in
  when none is selected. Zed's ctrl-o asks for files only, so this is how to
  open a project folder from it. Enter and a double-click still go into a
  folder, so a folder is never picked by accident. A folder picker has no
  Open Folder, because its accept button picks folders already.
- An archive is a file to pick. The picker does not go into one, because
  nothing in an archive has a path to give to another application.

The filter menu shows the filters the application gave. A filter hides the
files that do not match it, by name, and never hides a folder. Escape clears
the selection, and when nothing is selected it cancels. Closing the window
cancels too.

### Where it saves

- A save of one file has a name field in the footer. It starts with the name
  the application gave, with the name selected and the extension not, and it
  has the keyboard first. Enter in it saves. Tab moves the keyboard to the
  files.
- Select a file to take its name. Enter or a double-click on a file saves
  over it.
- Before a save replaces a file, Hoja asks. Cancel goes back to the dialog.
- The name can be a path: `../draft.txt`, `sub/draft.txt`, or one from `~`.
  A folder name goes into that folder, and the field gets the name it
  started with back.
- The filter menu works as when opening. The application is told the filter
  in use, and some, Firefox among them, save in the format it names.
- A save of several files asks for a folder only, and shows no files. The
  answer is each name the application gave, in the folder you are in, or in
  the one folder selected. If some of the names are there already, Hoja asks
  once for all of them.
- Nothing is saved in an archive.

A transfer still running when you pick or cancel stops with the picker. The
next Hoja to start offers to finish it, as after a crash.

### `hoja --pick`

`hoja --pick [PATH]` reads one request as JSON on stdin. Every field can be
left out, and empty stdin is the default request: one file, from `PATH`.
When `PATH` is a file, it starts selected.

```json
{ "title": "Open File", "accept_label": "_Open", "multiple": false,
  "directory": false,
  "filters": [ { "name": "Images", "globs": ["*.png"], "mimes": ["image/*"] } ],
  "current_filter": 0, "current_folder": "/home/me/Pictures" }
```

On a pick it writes one line of JSON on stdout and exits with 0. The URIs are
`file://` URIs, escaped as GLib escapes them. `filter` is the index of the
filter in use, or `null`.

```json
{"uris":["file:///home/me/Pictures/photo.png"],"filter":0}
```

On a cancel it writes nothing and exits with 1.

A save adds `"save": true`. `current_name` is the name to start with.
`current_file` is a file to save over: its folder is where to start, and its
name the name, unless `current_folder` and `current_name` say otherwise.
`files` makes it a save of several files, to one folder:

```json
{ "save": true, "current_name": "notes.txt", "current_folder": "/home/me" }
{ "save": true, "files": ["a.txt", "b.txt"] }
```

The answer is the same line, with the path of each file to write. A file
there may not exist yet.

## File operations

### Optimized transfers

Hoja automatically chooses the fastest correct method for every transfer:
`rename()` for moves on the same filesystem, reflinks on supported
filesystems, and `copy_file_range()` elsewhere with fallbacks. Sparse files,
permissions, timestamps, extended attributes, symlinks, and hardlinks are
preserved.

### Undo

Press `ctrl-z` to take back the last thing you did, whether that was a delete
or a transfer. `delete` moves files to the freedesktop trash, which is instant
on the same filesystem and compatible with other Linux file managers. Undoing
a transfer removes what it copied and restores anything it replaced. It checks
first: a file edited since the transfer is left alone and reported, and what
it removes goes to the trash rather than being deleted, so an undo can itself
be undone.

### Drag and drop

Drag files between panes, onto folders, or into other applications. Moves stay
on one filesystem, copies cross filesystems, and modifier keys override the
default behavior.

### Clipboard integration

Copy and paste files between Hoja and other file managers using the standard
GNOME clipboard format.

### Transfer progress

Progress reports bytes copied, transfer speed, time remaining, and file counts
from the start of the operation. Fixed-width columns keep everything from
shifting while values change.

### Transfer errors

Failed transfers stay in the progress list with a warning. Open the details to
see every failed file and its error.

### Pause and resume

Every transfer has a pause control, and `ctrl-shift-space` stops or starts
them all. A transfer pauses between files, so one already in flight finishes
first; the row says `pausing…` until it has actually stopped.

### Interrupted transfers

If Hoja is killed partway through a transfer, the next start clears up the
half-written files it left and offers to finish what it had not done.

### Desktop notifications

Long-running transfers notify you when they finish, and failures always
generate a notification using the standard freedesktop notification service.

## Navigation and search

### Recursive search

Press `ctrl-f` to search every directory below the current one. Results appear
as they are found. The first result is ready immediately: use the arrow keys
to move through them and `enter` to open one, all without leaving the search
field. It works inside an archive too, where there is nothing to walk: the
member list the pane already read answers immediately, and results keep
arriving if the archive is still being read. Each result is labeled by where
it sits, and copying one out keeps the structure below the folder you
searched.

### Places

Press `ctrl-p` to jump to your home directory, bookmarks, or attached drives.
Hoja reads the same bookmarks used by GTK file dialogs and can mount or eject
removable drives.

### Command palette

Press `ctrl-shift-p` to search available commands. Frequently used commands
naturally rise to the top. Each shows its key binding, and the palette looks
and behaves like Zed's: the arrow keys move the selection, and a click or
`enter` runs the command.

## Appearance

### Human-friendly dates

The Modified column shows relative times such as `just now`, `3 hours ago`, or
`2 months ago`.

### Columns you choose

A pane shows the size, the kind and the modified time. It can also show the
permissions, the owner and the group. Use the Columns list in the view menu to
show a column or to hide one. Click a header to sort by that column, and drag
a header sideways to move the column. A new pane from a split shows the same
columns as the pane it came from.

You can also set the order in the settings file with a list. A column you hide
keeps its place until you close Hoja. After that it comes back in the usual
place, because a column that is not shown has no position to record.

### Git status

File names are colored by Git status, as in Zed's project panel and with the
same theme colors: added, modified, deleted, a conflict, and ignored names
dimmed. A folder takes the status of what is in it, except that an ignored
file does not dim the folder that holds it. The status comes from Git itself,
so Hoja always matches the command line, and it follows a commit, an add or a
checkout made in a terminal.

### Themes

Hoja reads Zed theme files from `~/.config/hoja/themes/` and reloads them
automatically. Rosé Pine themes are included. The interface and number fonts
are settings too.

### Icons

File icons follow the Zed icon system and inherit colors from the active
theme. Zed icon themes color them: Hoja loads the icon themes of the Zed
extensions installed on the machine, and any in `~/.config/hoja/icon_themes/`.
Name one with `icon_theme`, or leave it out and Hoja uses the one Zed is set
to. A family name, such as "Colored Zed Icons Theme", takes its dark or light
variant to match the color theme.

### Settings

Configure Hoja with `~/.config/hoja/settings.json`. Hoja never rewrites the
file, preserves your comments, and reloads changes automatically.
