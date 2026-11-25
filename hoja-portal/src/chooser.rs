//! `org.freedesktop.impl.portal.FileChooser`.
//!
//! Each of the three methods runs one `hoja --pick`. A save asks with
//! `"save": true`, and a save of several files gives their names as `files`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use futures_lite::FutureExt as _;
use zbus::interface;
use zbus::object_server::ObjectServer;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// The portal's answers.
const SUCCESS: u32 = 0;
const CANCELLED: u32 = 1;
const FAILED: u32 = 2;

type Options = HashMap<String, OwnedValue>;
type Results = HashMap<String, OwnedValue>;
/// A filter as the portal sends it: a name, and patterns, each a glob (0)
/// or a MIME type (1).
type Filter = (String, Vec<(u32, String)>);

pub struct FileChooser {
    pub hoja: PathBuf,
}

#[interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    async fn open_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> (u32, Results) {
        let request = Request::from_options(&title, &options, Kind::Open);
        self.answer(server, handle, request).await
    }

    async fn save_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> (u32, Results) {
        let request = Request::from_options(&title, &options, Kind::Save);
        self.answer(server, handle, request).await
    }

    async fn save_files(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> (u32, Results) {
        let request = Request::from_options(&title, &options, Kind::SaveFiles);
        self.answer(server, handle, request).await
    }
}

impl FileChooser {
    /// Run Hoja for `request`, and serve `Close` at its handle meanwhile.
    async fn answer(
        &self,
        server: &ObjectServer,
        handle: OwnedObjectPath,
        request: Request,
    ) -> (u32, Results) {
        let (close, closed) = async_channel::bounded(1);
        // The portal closes the request when the app gives up on it.
        if server.at(&handle, Close(close)).await.is_err() {
            return (FAILED, Results::new());
        }
        let answer = pick(&self.hoja, &request, closed).await;
        let _ = server.remove::<Close, _>(&handle).await;
        answer
    }
}

/// `org.freedesktop.impl.portal.Request`, served at a request's handle for
/// as long as the dialog is up. Closing it stops the Hoja that answers it.
struct Close(async_channel::Sender<()>);

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl Close {
    async fn close(&self) {
        let _ = self.0.try_send(());
    }
}

/// Run one `hoja --pick`, and turn what it says into the portal's answer.
async fn pick(
    hoja: &Path,
    request: &Request,
    closed: async_channel::Receiver<()>,
) -> (u32, Results) {
    let child = async_process::Command::new(hoja)
        .arg("--pick")
        .stdin(async_process::Stdio::piped())
        .stdout(async_process::Stdio::piped())
        .stderr(async_process::Stdio::inherit())
        // A closed request drops the future below, and with it the child.
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => {
            eprintln!("hoja-portal: {} could not start: {err}", hoja.display());
            return (FAILED, Results::new());
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use futures_lite::AsyncWriteExt as _;
        let _ = stdin.write_all(request.json.as_bytes()).await;
        // Dropped here, so Hoja reads to the end.
    }

    let finished = async { Some(child.output().await) };
    let canceled = async {
        let _ = closed.recv().await;
        None
    };
    match finished.or(canceled).await {
        None => (CANCELLED, Results::new()),
        Some(Err(err)) => {
            eprintln!("hoja-portal: {} failed: {err}", hoja.display());
            (FAILED, Results::new())
        }
        Some(Ok(output)) => match output.status.code() {
            Some(0) => answer(&String::from_utf8_lossy(&output.stdout), &request.filters),
            Some(1) => (CANCELLED, Results::new()),
            _ => (FAILED, Results::new()),
        },
    }
}

/// Which of the three methods a request came by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Save,
    SaveFiles,
}

/// A request as `hoja --pick` reads it, and the filters it was made from,
/// which the answer names one of.
#[derive(Debug)]
struct Request {
    json: String,
    filters: Vec<Filter>,
}

impl Request {
    fn from_options(title: &str, options: &Options, kind: Kind) -> Request {
        let flag = |key: &str| {
            options
                .get(key)
                .and_then(|value| bool::try_from(value).ok())
                .unwrap_or(false)
        };
        let filters: Vec<Filter> = options
            .get("filters")
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| Vec::<Filter>::try_from(value).ok())
            .unwrap_or_default();
        let current: Option<Filter> = options
            .get("current_filter")
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| Filter::try_from(value).ok());
        // By its value, as the spec says. A current filter that is not in the
        // list is still the one to use: it goes on the end.
        let mut filters = filters;
        let current_filter = current.map(|current| {
            filters
                .iter()
                .position(|filter| *filter == current)
                .unwrap_or_else(|| {
                    filters.push(current);
                    filters.len() - 1
                })
        });
        let path = |key: &str| {
            options
                .get(key)
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| Vec::<u8>::try_from(value).ok())
                .and_then(|bytes| path_from_bytes(&bytes))
        };
        // Only for a save of several. A name that is not UTF-8 is left out,
        // and the answer then names one file fewer than the app asked for.
        let files: Vec<String> = match kind {
            Kind::SaveFiles => options
                .get("files")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| Vec::<Vec<u8>>::try_from(value).ok())
                .unwrap_or_default()
                .iter()
                .filter_map(|bytes| path_from_bytes(bytes))
                .collect(),
            _ => Vec::new(),
        };

        let json = serde_json::json!({
            "title": (!title.is_empty()).then_some(title),
            "accept_label": options
                .get("accept_label")
                .and_then(|value| <&str>::try_from(value).ok()),
            "multiple": flag("multiple"),
            "directory": flag("directory"),
            "filters": filters.iter().map(|(name, patterns)| serde_json::json!({
                "name": name,
                "globs": patterns.iter().filter(|(kind, _)| *kind == 0).map(|(_, p)| p).collect::<Vec<_>>(),
                "mimes": patterns.iter().filter(|(kind, _)| *kind == 1).map(|(_, p)| p).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "current_filter": current_filter,
            "current_folder": path("current_folder"),
            "save": kind != Kind::Open,
            "current_name": options
                .get("current_name")
                .and_then(|value| <&str>::try_from(value).ok())
                .filter(|_| kind == Kind::Save),
            "current_file": path("current_file").filter(|_| kind == Kind::Save),
            "files": files,
        });
        Request {
            json: json.to_string(),
            filters,
        }
    }
}

/// A path the portal sends as bytes, with a NUL on the end.
fn path_from_bytes(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.is_empty() {
        return None;
    }
    // JSON holds text, so a path that is not UTF-8 is not passed on. Hoja
    // then opens its usual folder.
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// The portal's answer for what `hoja --pick` wrote: the last line that is
/// not empty, so that a library writing to stdout cannot break it.
fn answer(stdout: &str, filters: &[Filter]) -> (u32, Results) {
    let Some(line) = stdout.lines().rev().find(|line| !line.trim().is_empty()) else {
        return (CANCELLED, Results::new());
    };
    let Ok(answer) = serde_json::from_str::<serde_json::Value>(line) else {
        eprintln!("hoja-portal: Hoja's answer is not JSON: {line}");
        return (FAILED, Results::new());
    };
    let uris: Vec<String> = answer["uris"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|uri| uri.as_str())
        .filter(|uri| uri.starts_with("file://"))
        .map(str::to_string)
        .collect();
    if uris.is_empty() {
        return (CANCELLED, Results::new());
    }
    let mut results = Results::new();
    if let Ok(value) = OwnedValue::try_from(Value::from(uris)) {
        results.insert("uris".to_string(), value);
    }
    let filter = answer["filter"]
        .as_u64()
        .and_then(|ix| filters.get(ix as usize));
    if let Some(filter) = filter
        && let Ok(value) = OwnedValue::try_from(Value::from(filter.clone()))
    {
        results.insert("current_filter".to_string(), value);
    }
    (SUCCESS, results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: Vec<(&str, Value<'static>)>) -> Options {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_string(), OwnedValue::try_from(value).unwrap()))
            .collect()
    }

    fn images() -> Filter {
        (
            "Images".to_string(),
            vec![(0, "*.png".to_string()), (1, "image/*".to_string())],
        )
    }

    fn text() -> Filter {
        ("Text".to_string(), vec![(0, "*.txt".to_string())])
    }

    #[test]
    fn the_options_become_hojas_request() {
        let request = Request::from_options(
            "Pick",
            &options(vec![
                ("multiple", Value::from(true)),
                ("accept_label", Value::from("_Attach")),
                ("filters", Value::from(vec![text(), images()])),
                ("current_filter", Value::from(images())),
                (
                    "current_folder",
                    Value::from(b"/home/u/Pictures\0".to_vec()),
                ),
            ]),
            Kind::Open,
        );
        let json: serde_json::Value = serde_json::from_str(&request.json).unwrap();
        assert_eq!(json["title"], "Pick");
        assert_eq!(json["save"], false);
        assert_eq!(json["accept_label"], "_Attach");
        assert_eq!(json["multiple"], true);
        assert_eq!(json["directory"], false);
        assert_eq!(json["filters"][1]["name"], "Images");
        assert_eq!(json["filters"][1]["globs"][0], "*.png");
        assert_eq!(json["filters"][1]["mimes"][0], "image/*");
        assert_eq!(json["current_filter"], 1);
        assert_eq!(json["current_folder"], "/home/u/Pictures");
    }

    #[test]
    fn a_current_filter_not_in_the_list_is_added_to_it() {
        let request = Request::from_options(
            "",
            &options(vec![("current_filter", Value::from(images()))]),
            Kind::Open,
        );
        let json: serde_json::Value = serde_json::from_str(&request.json).unwrap();
        assert_eq!(json["current_filter"], 0);
        assert_eq!(request.filters, vec![images()]);
        assert!(json["title"].is_null());
    }

    #[test]
    fn a_save_becomes_hojas_request() {
        let request = Request::from_options(
            "Save As",
            &options(vec![
                ("current_name", Value::from("page.html")),
                ("current_file", Value::from(b"/home/u/page.html\0".to_vec())),
                ("filters", Value::from(vec![text()])),
            ]),
            Kind::Save,
        );
        let json: serde_json::Value = serde_json::from_str(&request.json).unwrap();
        assert_eq!(json["save"], true);
        assert_eq!(json["current_name"], "page.html");
        assert_eq!(json["current_file"], "/home/u/page.html");
        assert_eq!(json["files"], serde_json::json!([]));

        let request = Request::from_options(
            "",
            &options(vec![(
                "files",
                Value::from(vec![b"a.txt\0".to_vec(), b"b.txt\0".to_vec()]),
            )]),
            Kind::SaveFiles,
        );
        let json: serde_json::Value = serde_json::from_str(&request.json).unwrap();
        assert_eq!(json["save"], true);
        assert_eq!(json["files"], serde_json::json!(["a.txt", "b.txt"]));
        assert!(json["current_name"].is_null());
    }

    #[test]
    fn hojas_answer_becomes_the_portals() {
        let filters = vec![text(), images()];
        let (code, results) = answer(
            "a stray line\n{\"uris\":[\"file:///a.png\",\"file:///b.png\"],\"filter\":1}\n",
            &filters,
        );
        assert_eq!(code, SUCCESS);
        let uris = Vec::<String>::try_from(results["uris"].try_clone().unwrap()).unwrap();
        assert_eq!(uris, ["file:///a.png", "file:///b.png"]);
        let filter = Filter::try_from(results["current_filter"].try_clone().unwrap()).unwrap();
        assert_eq!(filter, images());
    }

    #[test]
    fn nothing_or_nonsense_on_stdout_is_not_a_pick() {
        assert_eq!(answer("", &[]).0, CANCELLED);
        assert_eq!(answer("\n  \n", &[]).0, CANCELLED);
        assert_eq!(answer("not json", &[]).0, FAILED);
        assert_eq!(answer("{\"uris\":[\"https://x\"]}", &[]).0, CANCELLED);
    }
}
