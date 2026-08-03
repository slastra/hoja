mod assets;
mod clipboard;
mod command_palette;
mod config;
mod conflict_dialog;
mod dir_pane;
mod failure_report;
mod file_menu;
mod fs;
mod git;
mod history;
mod icon;
mod measure;
mod notifications;
mod opener;
mod owners;
mod pane_group;
mod path_editor;
mod picker;
mod place_finder;
mod places;
mod probe;
#[allow(dead_code)] // not wired into a pane yet; see the module header
mod scrollbar;
mod search;
mod theming;
mod workspace;

use std::path::PathBuf;

use gpui::{App, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size};
use gpui_platform::application;
use theme::LoadThemes;

use assets::Assets;
use command_palette as palette;
use dir_pane::{
    ClearSelection, CursorDown, CursorUp, EditPath, ExtendDown, ExtendPageDown, ExtendPageUp,
    ExtendToBottom, ExtendToTop, ExtendUp, GoHome, GoUp, MoveDown, MovePageDown, MovePageUp,
    MoveToBottom, MoveToTop, MoveUp, NavBack, NavForward, OpenSelected, RenameSelected, SelectAll,
    StartSearch, ToggleHiddenFiles, ToggleSelection,
};
use file_menu::{Cancel as MenuCancel, Confirm as MenuConfirm, SelectNext, SelectPrevious};
use path_editor as ab;
use place_finder as places_finder;
use workspace::{
    ClosePane, Copy, Cut, DismissJobs, FocusNext, FocusPrevious, Paste, SplitRight, Undo, Workspace,
};

struct Args {
    dir: PathBuf,
    /// `--theme <name>`, else `$HOJA_THEME`, else the default dark theme.
    theme: Option<String>,
    list_themes: bool,
}

/// Usage: `pane [DIR] [--theme NAME] [--list-themes]`
fn parse_args() -> Args {
    let mut dir = None;
    let mut theme = std::env::var("HOJA_THEME").ok();
    let mut list_themes = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--theme" => theme = args.next(),
            "--list-themes" => list_themes = true,
            // A desktop launcher hands a file manager `file:///…` rather than
            // a path, so accept both.
            other => {
                dir = Some(clipboard::uri_to_path(other).unwrap_or_else(|| PathBuf::from(other)))
            }
        }
    }

    Args {
        dir: dir.unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"))
        }),
        theme,
        list_themes,
    }
}

fn main() {
    let args = parse_args();
    // Read before the window exists: the theme, the first pane's view, and the
    // window size all come from these.
    let settings = config::Settings::load();
    let state = config::State::load();

    application().with_assets(Assets).run(move |cx: &mut App| {
        // Sets up ThemeRegistry, FontFamilyCache, SystemAppearance, and GlobalTheme so
        // `cx.theme()` resolves. The asset source is what `svg()` and the registry read
        // bundled icons and themes through. It does NOT parse bundled theme JSON:
        // that's the two calls below.
        theme::init(LoadThemes::All(Box::new(Assets)), cx);
        if let Err(err) = theming::load_bundled_themes(cx) {
            eprintln!("[hoja] bundled themes failed to load: {err:#}");
        }
        theming::load_user_themes(cx);

        if args.list_themes {
            for name in theming::list_names(cx) {
                println!("{name}");
            }
            // `cx.quit()` before any window exists does not unwind the run loop, so
            // exit the process directly. This is a debug flag, not a real subcommand.
            std::process::exit(0);
        }

        // --theme beats $HOJA_THEME beats settings.json. Whichever wins, its
        // themes/ directory is watched so an edit applies without a restart.
        if let Some(name) = args.theme.clone().or_else(|| settings.theme.clone())
            && let Err(err) = theming::apply(&name, cx)
        {
            eprintln!("[hoja] theme {name:?} not available: {err}");
        }
        // Armed unconditionally: it re-applies whatever theme is current, so it
        // is useful even when the default one is in force.
        theming::watch_user_themes(cx);

        // All bindings are scoped to the "DirPane" key context, which each pane declares
        // via `.key_context(...)` on a div that also calls `.track_focus(...)`. Both are
        // required or the binding never resolves. Split/focus actions bubble past the
        // focused pane up to the workspace, which handles them.
        //
        // Space-separated keystrokes form a chord. `ctrl-` is literal Control: note
        // that `cmd`/`super`/`win` are all aliases for the *platform* modifier, which on
        // Linux is Super, a key Hyprland grabs. Use `ctrl-` or `secondary-`, never
        // `cmd-`.
        cx.bind_keys([
            KeyBinding::new("backspace", GoUp, Some("DirPane && !AddressBar")),
            KeyBinding::new("alt-up", GoUp, Some("DirPane && !AddressBar")),
            KeyBinding::new("alt-left", NavBack, Some("DirPane && !AddressBar")),
            KeyBinding::new("alt-right", NavForward, Some("DirPane && !AddressBar")),
            KeyBinding::new("alt-home", GoHome, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-l", EditPath, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-f", StartSearch, Some("DirPane && !AddressBar")),
            KeyBinding::new("f2", RenameSelected, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-h", ToggleHiddenFiles, Some("DirPane && !AddressBar")),
            KeyBinding::new("enter", OpenSelected, Some("DirPane && !AddressBar")),
            // Tab between panes, F3 to split, ctrl-w to close: the file manager
            // scheme, which goes back to Norton Commander, rather than the
            // editor one. The other directions are still actions and still
            // reachable by name in the palette, that is what the palette is
            // for, and a command used once a week should not own a chord.
            KeyBinding::new("tab", FocusNext, Some("DirPane && !AddressBar")),
            KeyBinding::new("shift-tab", FocusPrevious, Some("DirPane && !AddressBar")),
            KeyBinding::new("f3", SplitRight, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-w", ClosePane, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-c", Copy, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-x", Cut, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-v", Paste, Some("DirPane && !AddressBar")),
            KeyBinding::new("up", MoveUp, Some("DirPane && !AddressBar")),
            KeyBinding::new("down", MoveDown, Some("DirPane && !AddressBar")),
            KeyBinding::new("shift-up", ExtendUp, Some("DirPane && !AddressBar")),
            KeyBinding::new("shift-down", ExtendDown, Some("DirPane && !AddressBar")),
            KeyBinding::new("pageup", MovePageUp, Some("DirPane && !AddressBar")),
            KeyBinding::new("pagedown", MovePageDown, Some("DirPane && !AddressBar")),
            KeyBinding::new("shift-pageup", ExtendPageUp, Some("DirPane && !AddressBar")),
            KeyBinding::new(
                "shift-pagedown",
                ExtendPageDown,
                Some("DirPane && !AddressBar"),
            ),
            KeyBinding::new("home", MoveToTop, Some("DirPane && !AddressBar")),
            KeyBinding::new("end", MoveToBottom, Some("DirPane && !AddressBar")),
            KeyBinding::new("shift-home", ExtendToTop, Some("DirPane && !AddressBar")),
            KeyBinding::new("shift-end", ExtendToBottom, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-up", CursorUp, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-down", CursorDown, Some("DirPane && !AddressBar")),
            KeyBinding::new(
                "ctrl-space",
                ToggleSelection,
                Some("DirPane && !AddressBar"),
            ),
            KeyBinding::new("ctrl-a", SelectAll, Some("DirPane && !AddressBar")),
            KeyBinding::new("escape", ClearSelection, Some("DirPane && !AddressBar")),
            KeyBinding::new("delete", workspace::Delete, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-z", Undo, Some("DirPane && !AddressBar")),
            KeyBinding::new("ctrl-shift-d", DismissJobs, Some("DirPane && !AddressBar")),
            // The palette toggles from anywhere, including from inside itself,
            // so it is not scoped to the pane.
            KeyBinding::new("ctrl-shift-p", palette::Toggle, None),
            KeyBinding::new("ctrl-p", places_finder::Toggle, None),
            // Only the arrows: the query field carries the AddressBar context
            // inside the palette's, and the deeper context wins, so enter and
            // escape always reach the editor. The pickers listen for its
            // Committed and Cancelled instead.
            KeyBinding::new("down", picker::SelectNext, Some("picker")),
            KeyBinding::new("up", picker::SelectPrevious, Some("picker")),
            // Address-bar editing. Every binding here MUST have a matching
            // on_action listener on the editor div, or it silently falls
            // through to the (now-guarded) DirPane binding.
            KeyBinding::new("backspace", ab::Backspace, Some("AddressBar")),
            KeyBinding::new("delete", ab::Delete, Some("AddressBar")),
            KeyBinding::new("left", ab::Left, Some("AddressBar")),
            KeyBinding::new("right", ab::Right, Some("AddressBar")),
            KeyBinding::new("shift-left", ab::SelectLeft, Some("AddressBar")),
            KeyBinding::new("shift-right", ab::SelectRight, Some("AddressBar")),
            KeyBinding::new("ctrl-left", ab::WordLeft, Some("AddressBar")),
            KeyBinding::new("ctrl-right", ab::WordRight, Some("AddressBar")),
            KeyBinding::new("ctrl-shift-left", ab::SelectWordLeft, Some("AddressBar")),
            KeyBinding::new("ctrl-shift-right", ab::SelectWordRight, Some("AddressBar")),
            KeyBinding::new("shift-home", ab::SelectToHome, Some("AddressBar")),
            KeyBinding::new("shift-end", ab::SelectToEnd, Some("AddressBar")),
            KeyBinding::new("ctrl-backspace", ab::DeleteWordLeft, Some("AddressBar")),
            KeyBinding::new("ctrl-delete", ab::DeleteWordRight, Some("AddressBar")),
            KeyBinding::new("ctrl-a", ab::SelectAll, Some("AddressBar")),
            KeyBinding::new("home", ab::Home, Some("AddressBar")),
            KeyBinding::new("end", ab::End, Some("AddressBar")),
            KeyBinding::new("ctrl-c", ab::Copy, Some("AddressBar")),
            KeyBinding::new("ctrl-x", ab::Cut, Some("AddressBar")),
            KeyBinding::new("ctrl-v", ab::Paste, Some("AddressBar")),
            KeyBinding::new("enter", ab::Confirm, Some("AddressBar")),
            KeyBinding::new("escape", ab::CancelEdit, Some("AddressBar")),
            // Modal dialogs: their own context, so menu navigation cannot leak in.
            KeyBinding::new("escape", conflict_dialog::Cancel, Some("dialog")),
            KeyBinding::new("enter", conflict_dialog::Confirm, Some("dialog")),
            // Context-menu navigation, scoped to the "menu" key context.
            KeyBinding::new("escape", MenuCancel, Some("menu")),
            KeyBinding::new(
                "escape",
                failure_report::Dismiss,
                Some(failure_report::KEY_CONTEXT),
            ),
            KeyBinding::new("enter", MenuConfirm, Some("menu")),
            KeyBinding::new("down", SelectNext, Some("menu")),
            KeyBinding::new("up", SelectPrevious, Some("menu")),
        ]);

        // The XDG mime database and .desktop entries cost ~12ms to parse. Do it
        // before the first right-click asks for them.
        cx.background_spawn(async { opener::warm_caches() })
            .detach();

        let window_size = state
            .window
            .map(|w| size(px(w.width), px(w.height)))
            .unwrap_or_else(|| size(px(1100.0), px(700.0)));
        let bounds = Bounds::centered(None, window_size, cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Lets Hyprland window rules target this app.
                app_id: Some("hoja".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(args.dir.clone(), settings, state, window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
