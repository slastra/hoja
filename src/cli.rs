//! The command line: `hoja [OPTIONS] [PATH]...`.
//!
//! `parse` is a pure function over the arguments, so the tests do not touch
//! the process's own. `resolve` then checks each path on disk. The arguments
//! are `OsString`s: a name that is not UTF-8 is still a name Hoja can open.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::uri;

pub const HELP: &str = "\
Hoja, a file manager.

Usage: hoja [OPTIONS] [PATH]...

Arguments:
  [PATH]...  The folders to open, each in a tab, as paths or file:// URIs.
             A file opens its folder with the file selected.
             The default is your home folder.

Options:
      --theme NAME    Use this theme. $HOJA_THEME also sets it.
      --list-themes   Print the names of the installed themes, then exit.
      --pick          Pick files for another program. Reads a JSON request
                      on stdin and writes the picked URIs on stdout.
                      Exits with 0 on a pick, 1 on a cancel, 2 on an error.
                      Takes one PATH at most.
  -h, --help          Print this help, then exit.
  -V, --version       Print the version, then exit.

Settings are in ~/.config/hoja/settings.json.
";

/// `hoja 0.1.0`, and the revision when the build sets `HOJA_REVISION`. The
/// PKGBUILD sets it to the package version, for example `r246.9e7b913`, so a
/// bug report names the commit.
pub fn version() -> String {
    match option_env!("HOJA_REVISION").filter(|rev| !rev.is_empty()) {
        Some(rev) => format!("hoja {} ({rev})", env!("CARGO_PKG_VERSION")),
        None => format!("hoja {}", env!("CARGO_PKG_VERSION")),
    }
}

pub struct Args {
    /// The paths as given, URIs already made paths. Empty means the home
    /// folder.
    pub paths: Vec<PathBuf>,
    /// `--theme NAME`. `$HOJA_THEME` is applied by the caller.
    pub theme: Option<String>,
    pub list_themes: bool,
    /// `--pick`: answer one request for a file, read from stdin. See `pick`.
    pub pick: bool,
}

pub enum Command {
    Run(Args),
    Help,
    Version,
}

/// The arguments after the program name.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut parsed = Args {
        paths: Vec::new(),
        theme: None,
        list_themes: false,
        pick: false,
    };
    let mut args = args.into_iter();
    let mut options_done = false;

    while let Some(arg) = args.next() {
        let text = arg.to_str();
        if !options_done && let Some(text) = text {
            match text {
                "-h" | "--help" => return Ok(Command::Help),
                "-V" | "--version" => return Ok(Command::Version),
                "--list-themes" => {
                    parsed.list_themes = true;
                    continue;
                }
                "--pick" => {
                    parsed.pick = true;
                    continue;
                }
                "--theme" => {
                    let name = args.next().and_then(|name| name.into_string().ok());
                    parsed.theme = Some(theme_name(name)?);
                    continue;
                }
                "--" => {
                    options_done = true;
                    continue;
                }
                _ => {}
            }
            if let Some(name) = text.strip_prefix("--theme=") {
                parsed.theme = Some(theme_name(Some(name.to_owned()))?);
                continue;
            }
            // A lone "-" is a name, not an option.
            if text.starts_with('-') && text != "-" {
                return Err(format!("unknown option {text}"));
            }
        }
        // A desktop launcher hands a file manager `file:///…` rather than a
        // path, so accept both.
        let path = text
            .and_then(uri::uri_to_path)
            .unwrap_or_else(|| PathBuf::from(arg));
        parsed.paths.push(path);
    }
    if parsed.pick && parsed.paths.len() > 1 {
        return Err("--pick takes one path at most".to_owned());
    }
    Ok(Command::Run(parsed))
}

fn theme_name(name: Option<String>) -> Result<String, String> {
    name.filter(|name| !name.is_empty())
        .ok_or_else(|| "--theme needs a theme name".to_owned())
}

/// Where one pane starts: a folder, and the file to select in it, when the
/// path given was a file.
#[derive(Debug, PartialEq)]
pub struct Start {
    pub dir: PathBuf,
    pub select: Option<PathBuf>,
}

/// Each path as a `Start`, made absolute against `cwd`. A path that is not
/// there, or cannot be read, is an error that names it, so nothing opens.
pub fn resolve(paths: &[PathBuf], cwd: &Path) -> Result<Vec<Start>, String> {
    paths
        .iter()
        .map(|path| {
            let absolute = normalize(&cwd.join(path));
            let meta = std::fs::metadata(&absolute).map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => format!("{}: no such file or folder", path.display()),
                _ => format!("{}: {err}", path.display()),
            })?;
            if meta.is_dir() {
                return Ok(Start {
                    dir: absolute,
                    select: None,
                });
            }
            // `/` is the only path without a parent, and it is a folder.
            let dir = absolute.parent().unwrap_or(Path::new("/")).to_path_buf();
            Ok(Start {
                dir,
                select: Some(absolute),
            })
        })
        .collect()
}

/// `.` and `..` taken out by their names, as a shell's `cd` does. The listing
/// keys rows by `dir.join(name)`, so the file to select has to be spelled the
/// same way as its folder, and the title should not read `/a/b/..`.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Args, String> {
        match parse(args.iter().map(OsString::from))? {
            Command::Run(args) => Ok(args),
            Command::Help => Err("help".to_owned()),
            Command::Version => Err("version".to_owned()),
        }
    }

    #[test]
    fn no_arguments_open_home() {
        let args = run(&[]).unwrap();
        assert!(args.paths.is_empty());
        assert!(args.theme.is_none());
        assert!(!args.pick && !args.list_themes);
    }

    #[test]
    fn help_and_version_win_over_other_arguments() {
        assert_eq!(run(&["/tmp", "--help"]).err().as_deref(), Some("help"));
        assert_eq!(run(&["-h"]).err().as_deref(), Some("help"));
        assert_eq!(
            run(&["--bogus", "-V"]).err().as_deref(),
            Some("unknown option --bogus")
        );
        assert_eq!(run(&["--version"]).err().as_deref(), Some("version"));
    }

    #[test]
    fn theme_takes_a_name_in_either_form() {
        assert_eq!(
            run(&["--theme", "Tint"]).unwrap().theme.as_deref(),
            Some("Tint")
        );
        assert_eq!(
            run(&["--theme=Rosé Pine"]).unwrap().theme.as_deref(),
            Some("Rosé Pine")
        );
        assert!(run(&["--theme"]).is_err());
        assert!(run(&["--theme="]).is_err());
    }

    #[test]
    fn unknown_options_are_errors() {
        assert_eq!(
            run(&["--them"]).err().as_deref(),
            Some("unknown option --them")
        );
        assert_eq!(run(&["-x"]).err().as_deref(), Some("unknown option -x"));
    }

    #[test]
    fn several_paths_are_kept_in_order_but_pick_takes_one() {
        let args = run(&["/b", "/a"]).unwrap();
        assert_eq!(args.paths, [PathBuf::from("/b"), PathBuf::from("/a")]);
        assert!(run(&["--pick", "/a"]).is_ok());
        assert!(run(&["--pick", "/a", "/b"]).is_err());
    }

    #[test]
    fn path_as_path_uri_or_after_double_dash() {
        assert_eq!(run(&["/tmp"]).unwrap().paths, [PathBuf::from("/tmp")]);
        assert_eq!(
            run(&["file:///tmp/a%20b"]).unwrap().paths,
            [PathBuf::from("/tmp/a b")]
        );
        assert_eq!(
            run(&["--", "--odd"]).unwrap().paths,
            [PathBuf::from("--odd")]
        );
        assert_eq!(run(&["-"]).unwrap().paths, [PathBuf::from("-")]);
        let args = run(&["--pick", "/tmp", "--list-themes"]).unwrap();
        assert!(args.pick && args.list_themes);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_kept() {
        use std::os::unix::ffi::OsStringExt;
        let name = OsString::from_vec(b"/tmp/caf\xe9".to_vec());
        let Ok(Command::Run(args)) = parse([name.clone()]) else {
            panic!("not parsed");
        };
        assert_eq!(args.paths, [PathBuf::from(name)]);
    }

    #[test]
    fn resolve_opens_folders_and_selects_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.txt"), "a").unwrap();

        let starts = resolve(
            &[PathBuf::from("sub"), PathBuf::from("./sub/../sub/a.txt")],
            root,
        )
        .unwrap();
        assert_eq!(
            starts,
            [
                Start {
                    dir: root.join("sub"),
                    select: None
                },
                Start {
                    dir: root.join("sub"),
                    select: Some(root.join("sub/a.txt"))
                },
            ]
        );
        // An absolute path ignores the working folder.
        let abs = resolve(&[root.join("sub")], Path::new("/nowhere")).unwrap();
        assert_eq!(abs[0].dir, root.join("sub"));
    }

    #[test]
    fn resolve_names_a_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve(&[PathBuf::from("gone")], tmp.path()).unwrap_err();
        assert_eq!(err, "gone: no such file or folder");
    }
}
