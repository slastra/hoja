# Testing the interface

The nested-compositor harness that drives Hoja's interface, and the suites that run in it.

`scripts/sway-harness.sh` runs Hoja in a compositor of its own, so a test of
the interface does not touch the desktop you are using. It takes a directory to
open and a script of things to do, and prints where the screenshots went:

```sh
cargo build
scripts/sway-harness.sh ~/some/dir my-test.sh
```

It needs `sway`, `grim`, `wtype` and `wlrctl`, and it exercises the Wayland
backend Hoja actually ships on. The test script can click rows, send keys, and
capture the window or just its footer; nothing takes your focus, because every
one of those tools is scoped to the nested display rather than to whatever the
real session has focused. `HOJA_TEST_WIDTH` and `HOJA_TEST_HEIGHT` size the
output when it is headless. Nested, the output takes the size of its window on
the host, so `scripts/readme-shots/run.sh`, which takes the README's pictures,
resizes that window with `hyprctl`. `HOJA_TEST_HEADLESS=1` hides it, at the
cost of keyboard input, which does not reach the app there.

Nested, its window presents to the host as class `wlroots`, so float it at a
fixed size or a tiling compositor will stretch it and move the rows a test
clicks.

A `wait_for` that times out ends the run. That is a safety property rather
than a tidiness one: once a wait has failed the script no longer knows where
the window is, and everything after it is keystrokes sent somewhere unverified.
An earlier version carried on, walked a pane up to the root of the filesystem
and pressed paste there; nothing was written only because root is not writable.

There are thirty-two suites in `scripts/tests/`. `listing.sh` runs against `~/Mock`;
`archive.sh` and `tar.sh` need fixtures, which `scripts/tests/setup-archives.sh`
builds and prints the path of:

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-archives.sh)" scripts/tests/archive.sh
scripts/sway-harness.sh "$(scripts/tests/setup-archives.sh)" scripts/tests/tar.sh
```

Rebuilt for each, not shared between them: `archive.sh` copies a folder out of
an archive and leaves it in the fixture directory, which is the tenth row
`tar.sh` does not expect. `setup-archives.sh` clears the directory before it
builds, so calling it twice is the whole fix.

`transfer.sh` needs `setup-transfer.sh`, and pauses a transfer by stopping it
on a conflict first: four thousand files copy in about 170ms here, so a test
that raced one would lose.

```sh
XFER=$(scripts/tests/setup-transfer.sh)
scripts/sway-harness.sh "$XFER" scripts/tests/transfer.sh
```

`context-menu.sh` needs `setup-context-menu.sh`, whose fixture is a PNG rather
than a text file on purpose: "Open With" is a submenu because the flat list was
capped at eight applications, and proving the cap is gone needs a type that
draws more than eight handlers. Images do; plain text does not.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-context-menu.sh)" scripts/tests/context-menu.sh
```

`columns.sh` needs `setup-columns.sh`, which sets the modes it asserts with
`chmod` rather than inheriting whatever `umask` gives it. The suite writes
`settings.json` while Hoja runs and waits for the header to follow, so it covers
the whole path a hand-written line takes without a coordinate in it.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-columns.sh)" scripts/tests/columns.sh
```

`search-click.sh` uses the same fixture. It covers what a search does when it
is used with the mouse and the arrows rather than only with enter.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-columns.sh)" scripts/tests/search-click.sh
```

`history.sh` needs `setup-history.sh`, which makes more folders than a window
shows. The suite goes into the last folder and back, and checks that the
cursor, the selection and the first row in view are where they were.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-history.sh)" scripts/tests/history.sh
```

`tabs.sh` needs `setup-tabs.sh`, which makes two folders. The suite opens tabs,
moves between them by key and by the bar, closes them, opens a folder in a tab
with a middle click, and drops a file on a tab. It also checks that a hidden
tab holds no watch, that a file written while its tab is hidden shows when the
tab is shown, and that Tab between panes passes over hidden tabs. The harness
finds a tab from the probe: `tab N`, `mtab N` and `tabclose N`.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-tabs.sh)" scripts/tests/tabs.sh
```

`tab-drag.sh` uses the same fixture. It drags tabs along a bar, to another
pane's bar, onto another pane, and to each edge of a pane to split. For each
edge, it holds the button down and reads the side that is highlighted from the
probe (`drag_split`) before it drops. It also does a ctrl drop, which copies.
The drags are slow and in the background, so a check can run during one; the
probe's `dragging` tells the test when the drag has started.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-tabs.sh)" scripts/tests/tab-drag.sh
```

`cli-start.sh` uses the same fixture. Its `.before.sh` puts a file from each
folder in `HOJA_ARGS`, and the harness adds the fixture folder last. The suite
checks that the three paths are three tabs in order, that each file's tab
shows its folder with the file selected, and that the folder's tab has nothing
selected.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-tabs.sh)" scripts/tests/cli-start.sh
```

`drag-external.sh` needs `setup-drag-external.sh` and nemo. It starts nemo in
the nested sway beside Hoja and drags a file out to it, a file in from it, and
a member out of a zip. nemo gets a private D-Bus and a keyfile settings
backend, so it does not touch the session that the test runs in.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-drag-external.sh)" scripts/tests/drag-external.sh
```

`preview.sh` needs `setup-preview.sh`, which makes an image, a video and an
audio file with ffmpeg, writes a two-page PDF, and also writes a text file and
a binary file. The
suite rests the pointer on each row and reads the preview's kind from the
probe. It also checks that the pictures went into the thumbnail cache and that
a second look comes from the cache. For MP4, the suite installs a thumbnailer
that waits two seconds, so that it can see the place that is kept for the
picture. The fixture includes a video turned a quarter, to show that the place
has the shape of the turned picture. The harness points `XDG_CACHE_HOME` into
the out-dir, so the test does not write to the cache that the desktop shares.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-preview.sh)" scripts/tests/preview.sh
```

`model-preview.sh` needs `setup-model-preview.sh` and a debug build of the
whole workspace (`cargo build --workspace`). The fixture holds STL, 3MF and
G-code files, written by Python, and a copy of a system font. The suite reads
the kind of each preview from the probe. It checks the sizes of the pictures
in the cache, so that a picture embedded in a 3MF cannot pass for a drawn
mesh, and it checks that G-code with no picture shows its text. It uses the
model thumbnailer from `target/debug`, not an installed one.

```sh
cargo build --workspace
scripts/sway-harness.sh "$(scripts/tests/setup-model-preview.sh)" scripts/tests/model-preview.sh
```

A suite `NAME.sh` can have a `NAME.before.sh` beside it. The harness runs it
before Hoja starts, with `OUT` and the private XDG directories set.
`model-preview.sh` and `preview.sh` use it to install thumbnailers. Hoja reads
the thumbnailers one time, at its first look at a file, and the pointer can
rest on a row when Hoja starts. A thumbnailer that a suite installs after the
start is then too late.

A `.before.sh` can also set these, for a suite that does not start Hoja the
usual way:

- `HOJA_ARGS`: arguments before the start folder, such as `--pick`.
- `HOJA_STDIN`: a file for Hoja's stdin. The default is `/dev/null`.
- `APP_ID`: the app_id of the window to wait for. The default is `hoja`.
- `EXTRA_PIDS`: processes that it started, which the harness stops at the
  end.
- `BIN`: the program to start in place of `target/debug/hoja`.

Hoja's stdout goes to `$OUT/app.out` and its stderr to `$OUT/app.log`. When
Hoja exits, its exit status goes to `$OUT/app.status`. `wait_exit [SECS]`
waits for that and prints it, or prints `running`. `MAPPED_MS` is the time
the window took to appear.

`picker-open.sh`, `picker-multiple.sh`, `picker-folder.sh` and
`picker-cancel.sh` test `hoja --pick`. Each has a `.before.sh` that gives the
request on stdin. They need `setup-picker.sh`, which makes two pictures, a
text file, a model, a zip and a folder. They check:

- what a filter shows, and a filter chosen from the bar's menu;
- that a picker for one file selects one row;
- that a double-click goes into a folder and picks a file;
- the accept button for several files, with a zip picked as a file;
- that a folder picker shows no files and picks the folder;
- that the first escape clears the selection and the second cancels.

Each one reads the answer from `app.out` and the exit status.

`portal.sh` tests the whole loop through D-Bus. Its `.before.sh` starts a
private `dbus-daemon` with no service directories, starts
`target/debug/hoja-portal` on it, and puts a `gdbus call` in place of the
app. The portal starts the debug Hoja. The suite picks two files and checks
the reply that the call writes. It needs a debug build of the whole
workspace.

```sh
cargo build --workspace
scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-open.sh
scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/portal.sh
```

`grid.sh` needs `setup-grid.sh`, which makes more than a screen of images in
five shapes, a video in each orientation, a PDF, audio, text and folders. The
suite switches to the grid layout and checks that pictures arrive, that the
arrows move by card and by band, that End and Home scroll, that a right-click
and a double-click work on a card, and that Back keeps the grid. The probe
gives each card in view as `[index, x, y, width, height, picture]`, and the
harness helpers `card`, `dcard` and `rcard` click a card by its index. Run it
nested, because it types. It also uses the toolbar's layout buttons and the
view menu's Card Details list. `grid-restore.sh` is its second half: a
restart against the state the first half left, which must still be a grid
with the same card details.

```sh
FIX=$(scripts/tests/setup-grid.sh)
OUT=$(mktemp -d)
scripts/sway-harness.sh "$FIX" scripts/tests/grid.sh "$OUT"
HOJA_TEST_KEEP_STATE=1 scripts/sway-harness.sh "$FIX" scripts/tests/grid-restore.sh "$OUT"
```

`git.sh` needs `setup-git.sh`, which makes a repository with a change deep in
a folder, a new file, an ignored folder, and a tracked folder that holds only
an ignored file. The probe gives each row's git status. The suite checks what
each row shows, then commits in the test body and checks the colours clear,
which only the watch on the git directory can see.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-git.sh)" scripts/tests/git.sh
```

`palette.sh` uses the git fixture for its five rows. It runs "pane: reverse
sort" from the command palette twice, once with the footer's Run button and
once by clicking the row, and checks the row order flips each time. It finds
Run in a screenshot.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-git.sh)" scripts/tests/palette.sh
```

`grid-big.sh` needs `setup-grid-big.sh`, which writes 1500 small images in
seven shapes with plain Python, more than the preview cache keeps. The suite
pages down the grid and reads the probe every 50ms for 2.5s after each page,
with `watch-shifts.py`, counting any card that moves relative to the others
while the view is still. It checks paging past the cache's size, a jump to
the end, and the way back to the top. It takes about two minutes.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-grid-big.sh)" scripts/tests/grid-big.sh
```

`icon-theme.sh` uses the grid fixture. It writes an icon theme of its own,
with a dark and a light variant, into the harness's private config, so it
does not depend on what is installed. It checks that a family name takes the
variant that matches the colour theme, that an icon is drawn in the theme's
own colour, and that removing the setting puts the default icons back.

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-grid.sh)" scripts/tests/icon-theme.sh
```

The harness drives a drag with `scripts/vpointer`, a small tool that presses,
moves and releases a virtual pointer. `wlrctl` cannot do this: its press and
release are one call. The harness builds the tool into `target/vpointer` the
first time a script calls `drag` or `drag_at`.

`crash-phase1.sh` and `crash-phase2.sh` are one test in two runs against one
state directory. The harness kills the app when a script returns, which is the
crash; `HOJA_TEST_KEEP_STATE=1` stops the second run wiping what the first
left behind.

```sh
OUT=$(mktemp -d)
scripts/sway-harness.sh "$XFER" scripts/tests/crash-phase1.sh "$OUT"
HOJA_TEST_KEEP_STATE=1 \
    scripts/sway-harness.sh "$XFER" scripts/tests/crash-phase2.sh "$OUT"
```

`resort-while-reading.sh`, `interrupt-archive-read.sh`,
`search-while-reading.sh` and `select-while-reading.sh` all act while an
archive is still being read, so they
need a fixture slow enough to act *in*. `setup-slow-archive.sh` builds it, and
is kept separate so a ~15 MB member genuinely slow to read (bzip2, real
pseudo-random content) does not change the row counts the other suites assert
on:

```sh
SLOW=$(scripts/tests/setup-slow-archive.sh)
scripts/sway-harness.sh "$SLOW" scripts/tests/resort-while-reading.sh
scripts/sway-harness.sh "$SLOW" scripts/tests/interrupt-archive-read.sh
scripts/sway-harness.sh "$SLOW" scripts/tests/search-while-reading.sh
scripts/sway-harness.sh "$SLOW" scripts/tests/select-while-reading.sh
```

`extract-cancel.sh` copies a file out of an archive and cancels it from the
strip while bzip2 is still working through the 80 MB in front of it.
`setup-slow-extract.sh` builds that archive, apart from the one above, which
other suites read whole:

```sh
scripts/sway-harness.sh "$(scripts/tests/setup-slow-extract.sh)" scripts/tests/extract-cancel.sh
```

Two things it cannot do. It cannot synthesise a drag: `wlrctl` only clicks, and
`swaymsg seat - cursor` moves the pointer without gpui starting a drag from it,
so column resizing and drag-and-drop need a person. And it has no second
application to drag from or paste into, so inbound drops and clipboard interop
have to be tried in a real session.

One thing to know if the nested window is sent to another workspace by a rule:
give it `render_unfocused` too. A window nobody is looking at gets no frame
callbacks, so the nested compositor paints nothing, hoja never renders past
its first frame, and every assertion times out against a listing that stays
empty.

`scripts/x11-harness.sh` is kept but does not currently run: `Xvfb` offers no
DRI3, gpui never gets a GPU context, and the window never opens. Testing the
X11 backend needs a real X server.
