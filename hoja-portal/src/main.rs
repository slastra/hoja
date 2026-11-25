//! `hoja-portal`: the desktop portal's file chooser, answered by Hoja.
//!
//! An app asks `xdg-desktop-portal` for a file. The portal calls the backend
//! that the user's `portals.conf` names for
//! `org.freedesktop.impl.portal.FileChooser`, and when that is this program,
//! it starts `hoja --pick` with the request on stdin and gives back what Hoja
//! writes on stdout. See `chooser` for the mapping.
//!
//! D-Bus starts this program at the first request, from its `.service` file,
//! and it stays running after that. It is small and holds nothing between
//! requests.

mod chooser;

use std::path::PathBuf;

/// The name the `.portal` file gives as `DBusName`.
const NAME: &str = "org.freedesktop.impl.portal.desktop.hoja";
/// Where every portal backend serves its interfaces.
const PATH: &str = "/org/freedesktop/portal/desktop";

const HELP: &str = "\
The desktop portal's file chooser, answered by Hoja. D-Bus starts it when
an application asks for a file, if the portal config names hoja for
org.freedesktop.impl.portal.FileChooser. It runs `hoja --pick` for each
request, to open files and to save them.

Usage: hoja-portal [OPTIONS]

Options:
  -h, --help     Print this help, then exit.
  -V, --version  Print the version, then exit.

Environment:
  HOJA_PORTAL_HOJA  The hoja program to run. The default is hoja on PATH.
";

/// As `hoja --version` gives it. The PKGBUILD sets `HOJA_REVISION` for the
/// whole build.
fn version() -> String {
    match option_env!("HOJA_REVISION").filter(|rev| !rev.is_empty()) {
        Some(rev) => format!("hoja-portal {} ({rev})", env!("CARGO_PKG_VERSION")),
        None => format!("hoja-portal {}", env!("CARGO_PKG_VERSION")),
    }
}

fn main() {
    // No arguments but these two. D-Bus starts it with none.
    if let Some(arg) = std::env::args_os().nth(1) {
        match arg.to_str() {
            Some("-h" | "--help") => print!("{HELP}"),
            Some("-V" | "--version") => println!("{}", version()),
            _ => {
                eprintln!("hoja-portal: unknown argument {}", arg.display());
                eprintln!("Try 'hoja-portal --help'.");
                std::process::exit(2);
            }
        }
        return;
    }
    // `HOJA_PORTAL_HOJA` names another Hoja, for a test or a build in a
    // source tree. Otherwise the one on PATH.
    let hoja = std::env::var_os("HOJA_PORTAL_HOJA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("hoja"));

    let served = async_io::block_on(async {
        let _connection = zbus::connection::Builder::session()?
            .name(NAME)?
            .serve_at(PATH, chooser::FileChooser { hoja })?
            .build()
            .await?;
        // The connection answers on its own executor thread. This one only
        // has to stay alive.
        std::future::pending::<()>().await;
        zbus::Result::Ok(())
    });
    if let Err(err) = served {
        eprintln!("hoja-portal: {err}");
        std::process::exit(1);
    }
}
