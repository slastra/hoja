# Configuration

Where Hoja keeps what you change through the interface. The [README](../README.md#configuration) covers the settings file.

Hoja keeps what you change through the interface (the sort order, hidden
files, the columns you show, and the column widths you drag) in
`~/.local/state/hoja/state.json`,
and reads it back at the next start. It writes that file and you write the
other one, so neither can overwrite the other. When both have an answer, the
more recent one applies: what you last toggled survives a restart, and editing
the settings file takes effect over it.

Two Hoja windows share that file safely. Each one writes only the settings you
changed in it, so a change made in one window is not undone by the other.
