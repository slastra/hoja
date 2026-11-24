//! 3MF: a zip that holds an XML model, and often a picture of it.
//!
//! The picture comes first: a slicer writes one into every project it saves,
//! and reading it costs much less than drawing the mesh. Without one, the
//! mesh is put together as the 3MF core spec says:
//!
//! - an `<object>` holds a `<mesh>`, or `<components>` that place other
//!   objects with a transform;
//! - a component can name an object in another model part of the package
//!   (`p:path`, the production extension). Bambu Studio and Orca put each
//!   object in its own part this way;
//! - `<build>` lists the `<item>`s to print, each with a transform.

use std::collections::HashMap;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use quick_xml::events::{BytesStart, Event};

use crate::Error;
use crate::render::{Point, Triangle};

pub enum Found {
    Picture(image::DynamicImage),
    Mesh(Vec<Triangle>),
}

const THUMBNAIL: &str = "/metadata/thumbnail";
const MODEL: &str = "/3dmodel";
/// Where the slicers put the picture, for a package whose relationships do
/// not name it.
const PICTURES: &[&str] = &[
    "Metadata/thumbnail.png",
    "Metadata/plate_1.png",
    "Metadata/top_1.png",
];
/// Components inside components, deeper than any real file goes. It stops a
/// package whose components name each other in a loop.
const DEPTH: usize = 16;

pub fn read(path: &Path) -> Result<Found, Error> {
    let file = BufReader::new(std::fs::File::open(path)?);
    let mut zip = zip::ZipArchive::new(file)?;
    let relationships = entry(&mut zip, "_rels/.rels")
        .ok()
        .map(|bytes| relationships(&bytes))
        .unwrap_or_default();

    let named = relationships
        .iter()
        .filter(|(kind, _)| kind.to_ascii_lowercase().ends_with(THUMBNAIL))
        .map(|(_, target)| target.as_str());
    for name in named.chain(PICTURES.iter().copied()) {
        if let Ok(bytes) = entry(&mut zip, name)
            && let Ok(picture) = image::load_from_memory(&bytes)
        {
            return Ok(Found::Picture(picture));
        }
    }

    let root = relationships
        .iter()
        .find(|(kind, _)| kind.to_ascii_lowercase().ends_with(MODEL))
        .map_or("3D/3dmodel.model", |(_, target)| target.as_str());
    let mesh = assemble(&mut zip, &part_name(root))?;
    if mesh.is_empty() {
        return Err("the 3MF has no mesh to draw".into());
    }
    Ok(Found::Mesh(mesh))
}

/// A part's name as the zip lists it: no leading slash.
fn part_name(name: &str) -> String {
    name.trim_start_matches('/').to_string()
}

/// The bytes of a part, found by name, or by name in any case: a package
/// written on Windows does not always keep the case its relationships use.
fn entry<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>, Error> {
    let name = part_name(name);
    let index = match zip.index_for_name(&name) {
        Some(index) => index,
        None => zip
            .file_names()
            .position(|n| n.is_ok_and(|n| n.eq_ignore_ascii_case(&name)))
            .ok_or("no such part")?,
    };
    let mut bytes = Vec::new();
    zip.by_index(index)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// `(Type, Target)` of each relationship.
fn relationships(xml: &[u8]) -> Vec<(String, String)> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    while let Ok(event) = reader.read_event_into(&mut buf) {
        match event {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"Relationship" => {
                if let (Some(kind), Some(target)) = (attr(&e, b"Type"), attr(&e, b"Target")) {
                    out.push((kind, target));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

fn number(e: &BytesStart, name: &[u8]) -> Option<f32> {
    let a = e
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)?;
    std::str::from_utf8(&a.value).ok()?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Transforms

/// A 3MF transform: twelve numbers, the first nine a 3×3 matrix by rows and
/// the last three the translation, applied to a point as a row vector.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Affine([f32; 12]);

impl Affine {
    const IDENTITY: Affine = Affine([1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 0.]);

    fn parse(text: Option<String>) -> Affine {
        let Some(text) = text else {
            return Affine::IDENTITY;
        };
        let values: Vec<f32> = text
            .split_ascii_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        values.try_into().map_or(Affine::IDENTITY, Affine)
    }

    fn apply(&self, p: Point) -> Point {
        let m = &self.0;
        [
            p[0] * m[0] + p[1] * m[3] + p[2] * m[6] + m[9],
            p[0] * m[1] + p[1] * m[4] + p[2] * m[7] + m[10],
            p[0] * m[2] + p[1] * m[5] + p[2] * m[8] + m[11],
        ]
    }

    /// This transform, then `outer`.
    fn then(&self, outer: &Affine) -> Affine {
        let linear = |row: [f32; 3]| {
            let o = &outer.0;
            [
                row[0] * o[0] + row[1] * o[3] + row[2] * o[6],
                row[0] * o[1] + row[1] * o[4] + row[2] * o[7],
                row[0] * o[2] + row[1] * o[5] + row[2] * o[8],
            ]
        };
        let m = &self.0;
        let rows = [
            linear([m[0], m[1], m[2]]),
            linear([m[3], m[4], m[5]]),
            linear([m[6], m[7], m[8]]),
        ];
        let t = outer.apply([m[9], m[10], m[11]]);
        Affine([
            rows[0][0], rows[0][1], rows[0][2], rows[1][0], rows[1][1], rows[1][2], rows[2][0],
            rows[2][1], rows[2][2], t[0], t[1], t[2],
        ])
    }
}

// ---------------------------------------------------------------------------
// Model parts

#[derive(Default)]
struct Part {
    objects: HashMap<String, Object>,
    build: Vec<(String, Affine)>,
}

#[derive(Default)]
struct Object {
    vertices: Vec<Point>,
    triangles: Vec<[u32; 3]>,
    components: Vec<Component>,
}

#[derive(Clone)]
struct Component {
    object: String,
    transform: Affine,
    /// The part that holds the object, when it is not this one.
    part: Option<String>,
}

/// One model part, read as a stream.
fn parse_part(reader: impl std::io::BufRead) -> Result<Part, Error> {
    let mut reader = quick_xml::Reader::from_reader(reader);
    let mut buf = Vec::new();
    let mut part = Part::default();
    let mut open: Option<(String, Object)> = None;
    let mut in_build = false;
    loop {
        let event = reader.read_event_into(&mut buf)?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"vertex" => {
                        if let Some((_, object)) = &mut open {
                            let at = |n| number(e, n).unwrap_or(0.);
                            object.vertices.push([at(b"x"), at(b"y"), at(b"z")]);
                        }
                    }
                    b"triangle" => {
                        if let Some((_, object)) = &mut open {
                            let at = |n| number(e, n).map(|v| v as u32);
                            if let (Some(a), Some(b), Some(c)) = (at(b"v1"), at(b"v2"), at(b"v3")) {
                                object.triangles.push([a, b, c]);
                            }
                        }
                    }
                    b"object" => {
                        let id = attr(e, b"id").unwrap_or_default();
                        if empty {
                            part.objects.insert(id, Object::default());
                        } else {
                            open = Some((id, Object::default()));
                        }
                    }
                    b"component" => {
                        if let Some((_, object)) = &mut open
                            && let Some(id) = attr(e, b"objectid")
                        {
                            object.components.push(Component {
                                object: id,
                                transform: Affine::parse(attr(e, b"transform")),
                                part: attr(e, b"path").map(|p| part_name(&p)),
                            });
                        }
                    }
                    b"build" => in_build = !empty,
                    b"item" if in_build => {
                        if let Some(id) = attr(e, b"objectid") {
                            part.build.push((id, Affine::parse(attr(e, b"transform"))));
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"object" => {
                    if let Some((id, object)) = open.take() {
                        part.objects.insert(id, object);
                    }
                }
                b"build" => in_build = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(part)
}

/// Every triangle the build places, in the package's coordinates.
fn assemble<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    root: &str,
) -> Result<Vec<Triangle>, Error> {
    let mut parts = HashMap::new();
    load(zip, root, &mut parts)?;
    let build = parts[root].build.clone();
    let mut out = Vec::new();
    for (id, transform) in &build {
        place(zip, &mut parts, root, id, *transform, 0, &mut out);
    }
    // A package with no build (not valid, but written) still has its
    // objects: draw each one where it stands.
    if build.is_empty() {
        let ids: Vec<String> = parts[root].objects.keys().cloned().collect();
        for id in ids {
            place(zip, &mut parts, root, &id, Affine::IDENTITY, 0, &mut out);
        }
    }
    Ok(out)
}

fn load<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
    parts: &mut HashMap<String, Part>,
) -> Result<(), Error> {
    if parts.contains_key(name) {
        return Ok(());
    }
    let index = zip
        .index_for_name(name)
        .or_else(|| {
            zip.file_names()
                .position(|n| n.is_ok_and(|n| n.eq_ignore_ascii_case(name)))
        })
        .ok_or("a model part is missing")?;
    let part = parse_part(BufReader::new(zip.by_index(index)?))?;
    parts.insert(name.to_string(), part);
    Ok(())
}

fn place<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    parts: &mut HashMap<String, Part>,
    part: &str,
    id: &str,
    transform: Affine,
    depth: usize,
    out: &mut Vec<Triangle>,
) {
    if depth > DEPTH || load(zip, part, parts).is_err() {
        return;
    }
    let Some(object) = parts[part].objects.get(id) else {
        return;
    };
    let vertex = |i: u32| object.vertices.get(i as usize).map(|&p| transform.apply(p));
    for &[a, b, c] in &object.triangles {
        if let (Some(a), Some(b), Some(c)) = (vertex(a), vertex(b), vertex(c)) {
            out.push([a, b, c]);
        }
    }
    let components = object.components.clone();
    for component in components {
        let at = component.part.as_deref().unwrap_or(part).to_string();
        let transform = component.transform.then(&transform);
        place(zip, parts, &at, &component.object, transform, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A 3MF package of `(name, bytes)` parts.
    fn package(parts: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(file.reopen().unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in parts {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        file
    }

    const RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
 <Relationship Target="/Metadata/cover.png" Id="rel1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"/>
</Relationships>"#;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(w, h)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn the_picture_the_relationships_name_comes_first() {
        let cover = png(7, 5);
        let other = png(3, 3);
        let file = package(&[
            ("_rels/.rels", RELS),
            ("Metadata/cover.png", &cover),
            ("Metadata/thumbnail.png", &other),
        ]);
        let Found::Picture(picture) = read(file.path()).unwrap() else {
            panic!("no picture");
        };
        assert_eq!((picture.width(), picture.height()), (7, 5));
    }

    #[test]
    fn a_picture_where_the_slicers_put_it_is_found() {
        let plate = png(9, 9);
        let file = package(&[("Metadata/plate_1.png", &plate)]);
        assert!(matches!(read(file.path()).unwrap(), Found::Picture(_)));
    }

    const ROOT: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"
       xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06">
 <resources>
  <object id="1" type="model">
   <mesh>
    <vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices>
    <triangles><triangle v1="0" v2="1" v3="2"/></triangles>
   </mesh>
  </object>
  <object id="2" type="model">
   <components>
    <component objectid="1" transform="1 0 0 0 1 0 0 0 1 10 0 0"/>
    <component objectid="5" p:path="/3D/Objects/object_5.model" transform="2 0 0 0 2 0 0 0 2 0 0 0"/>
   </components>
  </object>
 </resources>
 <build><item objectid="2" transform="1 0 0 0 1 0 0 0 1 0 0 3"/></build>
</model>"#;

    const OBJECT_5: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
 <resources>
  <object id="5" type="model">
   <mesh>
    <vertices><vertex x="0" y="0" z="1"/><vertex x="1" y="0" z="1"/><vertex x="0" y="1" z="1"/></vertices>
    <triangles><triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="1" v3="9"/></triangles>
   </mesh>
  </object>
 </resources>
</model>"#;

    #[test]
    fn components_are_placed_through_every_transform() {
        let file = package(&[
            ("_rels/.rels", RELS),
            ("3D/3dmodel.model", ROOT),
            ("3D/Objects/object_5.model", OBJECT_5),
        ]);
        let Found::Mesh(mesh) = read(file.path()).unwrap() else {
            panic!("no mesh");
        };
        // The triangle with a vertex that is not there is left out.
        assert_eq!(
            mesh,
            vec![
                [[10., 0., 3.], [11., 0., 3.], [10., 1., 3.]],
                [[0., 0., 5.], [2., 0., 5.], [0., 2., 5.]],
            ]
        );
    }

    #[test]
    fn transforms_compose_inner_first() {
        let turn = Affine([0., 1., 0., -1., 0., 0., 0., 0., 1., 0., 0., 0.]);
        let shift = Affine([1., 0., 0., 0., 1., 0., 0., 0., 1., 5., 0., 0.]);
        let both = turn.then(&shift);
        assert_eq!(both.apply([1., 0., 0.]), shift.apply(turn.apply([1., 0., 0.])));
        assert_eq!(both.apply([1., 0., 0.]), [5., 1., 0.]);
    }

    #[test]
    fn a_package_with_nothing_to_draw_is_refused() {
        let file = package(&[("readme.txt", b"hello")]);
        assert!(read(file.path()).is_err());
    }
}
