//! A freedesktop thumbnailer for 3D printing files: STL, 3MF and G-code.
//!
//! ```text
//! hoja-model-thumbnailer -s SIZE INPUT OUTPUT
//! ```
//!
//! It writes a PNG to OUTPUT, no larger than SIZE on its long side, with a
//! transparent background. It exits non-zero when INPUT has no picture to give.
//! `packaging/hoja-model.thumbnailer` registers it for the types it takes.
//!
//! - **3MF and G-code:** a slicer often puts a picture of the model in the
//!   file. That picture is used when it is there, because it costs only a read.
//! - **STL, and 3MF with no picture:** the mesh is drawn on the CPU (see
//!   `render`). There is no GPU context to start, which is most of what a GPU
//!   thumbnailer spends on a small model.
//! - **G-code with no picture:** none is given. Rendering toolpaths is a job
//!   of its own, and a file manager shows G-code as text without one.

mod gcode;
mod render;
mod stl;
mod threemf;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("hoja-model-thumbnailer: {err}");
            eprintln!("usage: hoja-model-thumbnailer -s SIZE INPUT OUTPUT");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hoja-model-thumbnailer: {}: {err}", args.input.display());
            ExitCode::FAILURE
        }
    }
}

struct Args {
    size: u32,
    input: PathBuf,
    output: PathBuf,
}

impl Args {
    fn parse(args: impl Iterator<Item = OsString>) -> Result<Args, Error> {
        let mut size = 256;
        let mut paths = Vec::new();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            if arg == "-s" || arg == "--size" {
                let value = args.next().ok_or("-s needs a size")?;
                size = value
                    .to_str()
                    .and_then(|v| v.parse().ok())
                    .filter(|&s: &u32| (1..=4096).contains(&s))
                    .ok_or("the size is not a number from 1 to 4096")?;
            } else {
                paths.push(arg);
            }
        }
        let [input, output]: [OsString; 2] = paths
            .try_into()
            .map_err(|_| "give one input and one output")?;
        Ok(Args {
            size,
            input: from_uri(input),
            output: PathBuf::from(output),
        })
    }
}

/// A path, or a `file://` URI turned into one, for an `Exec` line that
/// passes `%u` rather than `%i`.
fn from_uri(arg: OsString) -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    let bytes = arg.into_vec();
    let Some(rest) = bytes.strip_prefix(b"file://") else {
        return PathBuf::from(OsString::from_vec(bytes));
    };
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if rest[i] == b'%'
            && let (Some(&a), Some(&b)) = (rest.get(i + 1), rest.get(i + 2))
            && let (Some(a), Some(b)) = (hex(a), hex(b))
        {
            out.push((a * 16 + b) as u8);
            i += 3;
        } else {
            out.push(rest[i]);
            i += 1;
        }
    }
    PathBuf::from(OsString::from_vec(out))
}

enum Format {
    Stl,
    ThreeMf,
    Gcode,
}

/// By content where the content says, else by name. A 3MF is a zip; STL has
/// no magic of its own, and G-code is text that could be anything.
fn format_of(path: &Path) -> Result<Format, Error> {
    use std::io::Read as _;
    let mut head = [0u8; 4];
    let read = std::fs::File::open(path)?.read(&mut head)?;
    if read == 4 && head == *b"PK\x03\x04" {
        return Ok(Format::ThreeMf);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    Ok(match ext.as_deref() {
        Some("gcode" | "gco" | "g") => Format::Gcode,
        _ => Format::Stl,
    })
}

fn run(args: &Args) -> Result<(), Error> {
    let picture = match format_of(&args.input)? {
        Format::Stl => {
            let bytes = std::fs::read(&args.input)?;
            render::render(&stl::parse(&bytes)?, args.size)?
        }
        Format::ThreeMf => match threemf::read(&args.input)? {
            threemf::Found::Picture(picture) => fit(picture, args.size),
            threemf::Found::Mesh(mesh) => render::render(&mesh, args.size)?,
        },
        Format::Gcode => fit(gcode::picture(&args.input)?, args.size),
    };
    picture.save_with_format(&args.output, image::ImageFormat::Png)?;
    Ok(())
}

/// Shrunk to fit `size` on its long side. Never enlarged.
fn fit(picture: image::DynamicImage, size: u32) -> image::RgbaImage {
    if picture.width() <= size && picture.height() <= size {
        picture.into_rgba8()
    } else {
        picture
            .resize(size, size, image::imageops::FilterType::Triangle)
            .into_rgba8()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_becomes_a_path() {
        assert_eq!(
            from_uri("file:///home/a%20b/c%25.stl".into()),
            PathBuf::from("/home/a b/c%.stl")
        );
        assert_eq!(from_uri("/plain/path".into()), PathBuf::from("/plain/path"));
    }

    #[test]
    fn the_arguments_are_the_spec_s() {
        let args = ["-s", "512", "in.stl", "out.png"].map(OsString::from);
        let args = Args::parse(args.into_iter()).unwrap();
        assert_eq!(args.size, 512);
        assert_eq!(args.input, PathBuf::from("in.stl"));
        assert_eq!(args.output, PathBuf::from("out.png"));
        assert!(Args::parse(["in.stl"].map(OsString::from).into_iter()).is_err());
    }
}
