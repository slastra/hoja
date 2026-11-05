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
window, which is how `docs/screenshot.png` is made. `HOJA_TEST_HEADLESS=1` hides
it, at the cost of keyboard input, which does not reach the app there.

Nested, its window presents to the host as class `wlroots`, so float it at a
fixed size or a tiling compositor will stretch it and move the rows a test
clicks.

A `wait_for` that times out ends the run. That is a safety property rather
than a tidiness one: once a wait has failed the script no longer knows where
the window is, and everything after it is keystrokes sent somewhere unverified.
An earlier version carried on, walked a pane up to the root of the filesystem
and pressed paste there; nothing was written only because root is not writable.

There are thirteen suites in `scripts/tests/`. `listing.sh` runs against `~/Mock`;
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
