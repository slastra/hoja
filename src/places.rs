//! Places the `ctrl-p` finder offers: home, bookmarks, and attached volumes.
//!
//! Bookmarks are read from the file the GTK file dialogs already use, so pane
//! inherits whatever is bookmarked in Nautilus and the two stay in step. There
//! is deliberately no bookmark store of pane's own: a second list that has to
//! be kept in sync by hand is worse than no list.
//!
//! Volumes come from `lsblk`, filtered on `hotplug` rather than the `rm` flag.
//! `rm` reflects the SCSI "medium is ejectable" bit and reads false on most USB
//! enclosures, this machine's own USB disk reports `rm=false, hotplug=true`,
//! which is the same trap `sys::is_removable` documents in the transfer engine.

use std::path::PathBuf;
use std::process::Command;

use crate::clipboard::uri_to_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// Somewhere a pane can be pointed at right now.
    Dir {
        label: String,
        detail: String,
        path: PathBuf,
    },
    /// Attached but not mounted. Choosing it mounts first, then navigates.
    Volume {
        label: String,
        detail: String,
        device: PathBuf,
    },
}

impl Place {
    pub fn label(&self) -> &str {
        match self {
            Place::Dir { label, .. } | Place::Volume { label, .. } => label,
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Place::Dir { detail, .. } | Place::Volume { detail, .. } => detail,
        }
    }
}

/// Home and bookmarks: a file read, cheap enough to do while opening the
/// finder. Volumes come separately through `volumes`, which shells out.
///
/// Ordering is the tie-break for equal match scores, so the most likely
/// destinations come first.
pub fn local() -> Vec<Place> {
    let mut places = Vec::new();

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        places.push(Place::Dir {
            label: "Home".to_string(),
            detail: home.display().to_string(),
            path: home,
        });
    }
    places.extend(bookmarks());
    places
}

fn bookmarks_path() -> Option<PathBuf> {
    Some(crate::config::config_home()?.join("gtk-3.0/bookmarks"))
}

fn bookmarks() -> Vec<Place> {
    let Some(path) = bookmarks_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_bookmarks(&text)
}

/// One `file://` URI per line, optionally followed by a space and a display
/// name. Remote entries (`sftp://`, `smb://`) are skipped: pane cannot open
/// them, and offering a destination that fails is worse than omitting it.
fn parse_bookmarks(text: &str) -> Vec<Place> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (uri, name) = match line.split_once(' ') {
                Some((uri, name)) => (uri, Some(name.trim())),
                None => (line, None),
            };
            let path = uri_to_path(uri)?;
            let label = name
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))?;
            Some(Place::Dir {
                detail: path.display().to_string(),
                label,
                path,
            })
        })
        .collect()
}

pub fn volumes() -> Vec<Place> {
    let out = Command::new("lsblk")
        .args(["-J", "-o", "PATH,LABEL,FSTYPE,SIZE,MOUNTPOINT,HOTPLUG"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|json| parse_volumes(&json))
        .unwrap_or_default()
}

/// Hotplug devices that hold a filesystem, mounted or not.
fn parse_volumes(json: &str) -> Vec<Place> {
    let Ok(root) = serde_json_lenient::from_str::<serde_json_lenient::Value>(json) else {
        return Vec::new();
    };
    let mut places = Vec::new();
    let mut queue: Vec<&serde_json_lenient::Value> = root
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .map(|d| d.iter().collect())
        .unwrap_or_default();

    while let Some(node) = queue.pop() {
        if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
            queue.extend(children);
        }
        let hotplug = node
            .get("hotplug")
            .and_then(|h| h.as_bool())
            .unwrap_or(false);
        let fstype = node.get("fstype").and_then(|f| f.as_str()).unwrap_or("");
        if !hotplug || fstype.is_empty() {
            continue;
        }

        let device = node.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let size = node.get("size").and_then(|s| s.as_str()).unwrap_or("");
        let label = node
            .get("label")
            .and_then(|l| l.as_str())
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| device.rsplit('/').next().unwrap_or(device).to_string());

        match node.get("mountpoint").and_then(|m| m.as_str()) {
            Some(mount) if !mount.is_empty() => places.push(Place::Dir {
                label,
                detail: format!("{size}  {mount}"),
                path: PathBuf::from(mount),
            }),
            _ => places.push(Place::Volume {
                label,
                detail: format!("{size}  not mounted"),
                device: PathBuf::from(device),
            }),
        }
    }
    // `queue.pop()` walks depth-first from the back, so restore document order.
    places.reverse();
    places
}

/// Mount a volume and return where it landed.
///
/// Shells out to `udisksctl`, which routes through polkit and mounts as the
/// user under /run/media, the same path Nautilus takes. Doing it ourselves
/// would need root.
pub fn mount(device: &std::path::Path) -> std::io::Result<PathBuf> {
    let out = Command::new("udisksctl")
        .args(["mount", "-b"])
        .arg(device)
        .output()?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(std::io::Error::other(if detail.is_empty() {
            format!("could not mount {}", device.display())
        } else {
            detail
        }));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_mount_output(&stdout).ok_or_else(|| {
        std::io::Error::other(format!(
            "mounted {} but could not tell where",
            device.display()
        ))
    })
}

/// `udisksctl` prints `Mounted /dev/sdc1 at /run/media/shaun/Backup`.
fn parse_mount_output(stdout: &str) -> Option<PathBuf> {
    let line = stdout.lines().find(|l| l.contains(" at "))?;
    let mount = line.rsplit_once(" at ")?.1.trim().trim_end_matches('.');
    (!mount.is_empty()).then(|| PathBuf::from(mount))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_take_their_label_or_their_name() {
        let text = "\
file:///home/shaun/Projects Projects
file:///home/shaun/Downloads
file:///home/shaun/My%20Docs
sftp://elsewhere/remote Remote

";
        let places = parse_bookmarks(text);
        assert_eq!(places.len(), 3, "the remote entry is skipped: {places:?}");
        assert_eq!(places[0].label(), "Projects");
        // No label given, so the directory name stands in.
        assert_eq!(places[1].label(), "Downloads");
        // Percent-escapes are decoded, via the clipboard's reader.
        assert_eq!(places[2].label(), "My Docs");
    }

    #[test]
    fn only_hotplug_devices_with_a_filesystem_count() {
        // Trimmed from this machine's real lsblk output.
        let json = r#"{"blockdevices":[
          {"path":"/dev/sda","hotplug":true,"fstype":null,"children":[
            {"path":"/dev/sda1","label":null,"fstype":"vfat","size":"931.5G",
             "mountpoint":"/run/media/shaun/F109-1E1F","hotplug":true}]},
          {"path":"/dev/sdc","hotplug":true,"fstype":null,"children":[
            {"path":"/dev/sdc1","label":"Backup","fstype":"ext4","size":"238.7G",
             "mountpoint":null,"hotplug":true}]},
          {"path":"/dev/nvme0n1","hotplug":false,"fstype":null,"children":[
            {"path":"/dev/nvme0n1p2","label":null,"fstype":"ext4","size":"930.5G",
             "mountpoint":"/","hotplug":false}]}]}"#;

        let places = parse_volumes(json);
        assert_eq!(
            places.len(),
            2,
            "the internal disk is not a place: {places:?}"
        );

        // Mounted: offered as a directory, named from the device when unlabelled.
        assert_eq!(
            places[0],
            Place::Dir {
                label: "sda1".to_string(),
                detail: "931.5G  /run/media/shaun/F109-1E1F".to_string(),
                path: PathBuf::from("/run/media/shaun/F109-1E1F"),
            }
        );
        // Unmounted: offered as a volume to mount, named from its label.
        assert_eq!(
            places[1],
            Place::Volume {
                label: "Backup".to_string(),
                detail: "238.7G  not mounted".to_string(),
                device: PathBuf::from("/dev/sdc1"),
            }
        );
    }

    #[test]
    fn reads_the_mount_point_back_out() {
        assert_eq!(
            parse_mount_output("Mounted /dev/sdc1 at /run/media/shaun/Backup.\n"),
            Some(PathBuf::from("/run/media/shaun/Backup"))
        );
        assert_eq!(parse_mount_output("something went wrong\n"), None);
    }
}
