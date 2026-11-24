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

fn main() {
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
