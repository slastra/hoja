//! The command line: `hoja [OPTIONS] [FOLDER]`.
//!
//! Parsing is a pure function over the arguments, so the tests do not touch
//! the process's own. The arguments are `OsString`s: a folder name that is
//! not UTF-8 is still a folder Hoja can open.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::uri;

pub const HELP: &str = "\
Hoja, a file manager.

Usage: hoja [OPTIONS] [FOLDER]

Arguments:
  [FOLDER]  The folder to open, as a path or a file:// URI.
            The default is your home folder.

Options:
      --theme NAME    Use this theme. $HOJA_THEME also sets it.
      --list-themes   Print the names of the installed themes, then exit.
      --pick          Pick files for another program. Reads a JSON request
                      on stdin and writes the picked URIs on stdout.
                      Exits with 0 on a pick, 1 on a cancel, 2 on an error.
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
    /// The folder to open. `None` means the home folder.
    pub dir: Option<PathBuf>,
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
        dir: None,
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
        if parsed.dir.is_some() {
            return Err("give one folder only".to_owned());
        }
        // A desktop launcher hands a file manager `file:///…` rather than a
        // path, so accept both.
        let path = text
            .and_then(uri::uri_to_path)
            .unwrap_or_else(|| PathBuf::from(arg));
        parsed.dir = Some(path);
    }
    Ok(Command::Run(parsed))
}

fn theme_name(name: Option<String>) -> Result<String, String> {
    name.filter(|name| !name.is_empty())
        .ok_or_else(|| "--theme needs a theme name".to_owned())
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
        assert!(args.dir.is_none());
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
    fn unknown_options_and_two_folders_are_errors() {
        assert_eq!(
            run(&["--them"]).err().as_deref(),
            Some("unknown option --them")
        );
        assert_eq!(run(&["-x"]).err().as_deref(), Some("unknown option -x"));
        assert!(run(&["/a", "/b"]).is_err());
    }

    #[test]
    fn folder_as_path_uri_or_after_double_dash() {
        assert_eq!(run(&["/tmp"]).unwrap().dir, Some(PathBuf::from("/tmp")));
        assert_eq!(
            run(&["file:///tmp/a%20b"]).unwrap().dir,
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(
            run(&["--", "--odd"]).unwrap().dir,
            Some(PathBuf::from("--odd"))
        );
        assert_eq!(run(&["-"]).unwrap().dir, Some(PathBuf::from("-")));
        let args = run(&["--pick", "/tmp", "--list-themes"]).unwrap();
        assert!(args.pick && args.list_themes);
    }

    #[test]
    fn a_folder_name_that_is_not_utf8_is_kept() {
        use std::os::unix::ffi::OsStringExt;
        let name = OsString::from_vec(b"/tmp/caf\xe9".to_vec());
        let Ok(Command::Run(args)) = parse([name.clone()]) else {
            panic!("not parsed");
        };
        assert_eq!(args.dir, Some(PathBuf::from(name)));
    }
}
