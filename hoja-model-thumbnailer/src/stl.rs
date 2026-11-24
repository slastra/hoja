//! STL, binary and ASCII.

use crate::Error;
use crate::render::{Mesh, Triangle};

/// A binary STL is drawn from the file's bytes, so a model of millions of
/// triangles is never copied into a list. An ASCII one is parsed into a list
/// once, because it is read twice and parsing text is the slow part.
pub enum Stl<'a> {
    Binary(&'a [u8]),
    Ascii(Vec<Triangle>),
}

const HEADER: usize = 80;
const RECORD: usize = 50;

pub fn parse(bytes: &[u8]) -> Result<Stl<'_>, Error> {
    // By the length, not by the word `solid`: many exporters start the
    // 80-byte header of a binary file with it.
    if let Some(count) = binary_count(bytes) {
        let end = HEADER + 4 + count * RECORD;
        if end == bytes.len() {
            return Ok(Stl::Binary(&bytes[HEADER + 4..end]));
        }
    }
    if starts_with_solid(bytes) {
        let triangles = ascii(bytes)?;
        if !triangles.is_empty() {
            return Ok(Stl::Ascii(triangles));
        }
    }
    // Some exporters leave bytes after the last record. Take the records the
    // count names when they are all there.
    if let Some(count) = binary_count(bytes) {
        let end = HEADER + 4 + count * RECORD;
        if count > 0 && end <= bytes.len() {
            return Ok(Stl::Binary(&bytes[HEADER + 4..end]));
        }
    }
    Err("not an STL file".into())
}

fn binary_count(bytes: &[u8]) -> Option<usize> {
    let count = bytes.get(HEADER..HEADER + 4)?;
    Some(u32::from_le_bytes(count.try_into().ok()?) as usize)
}

fn starts_with_solid(bytes: &[u8]) -> bool {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace());
    start.is_some_and(|i| bytes[i..].starts_with(b"solid"))
}

fn ascii(bytes: &[u8]) -> Result<Vec<Triangle>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| "an ASCII STL that is not text")?;
    let mut words = text.split_ascii_whitespace();
    let mut triangles = Vec::new();
    let mut corners = Vec::with_capacity(3);
    while let Some(word) = words.next() {
        if word != "vertex" {
            continue;
        }
        let mut point = [0f32; 3];
        for axis in &mut point {
            *axis = words
                .next()
                .and_then(|w| w.parse().ok())
                .ok_or("a vertex without three numbers")?;
        }
        corners.push(point);
        if corners.len() == 3 {
            triangles.push([corners[0], corners[1], corners[2]]);
            corners.clear();
        }
    }
    Ok(triangles)
}

impl Mesh for Stl<'_> {
    fn each(&self, f: &mut dyn FnMut(&Triangle)) {
        match self {
            Stl::Binary(records) => {
                for record in records.as_chunks::<RECORD>().0 {
                    // The normal comes first. It is not used: exporters often
                    // write zeros, or a normal that points the wrong way.
                    let mut triangle = [[0f32; 3]; 3];
                    for (corner, point) in triangle.iter_mut().enumerate() {
                        for (axis, value) in point.iter_mut().enumerate() {
                            let at = 12 + corner * 12 + axis * 4;
                            *value = f32::from_le_bytes(
                                record[at..at + 4].try_into().expect("four bytes"),
                            );
                        }
                    }
                    f(&triangle);
                }
            }
            Stl::Ascii(triangles) => triangles.iter().for_each(f),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A binary STL of `triangles`, with `header` at the front.
    pub(crate) fn binary(header: &[u8], triangles: &[Triangle]) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER];
        bytes[..header.len()].copy_from_slice(header);
        bytes.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
        for triangle in triangles {
            bytes.extend_from_slice(&[0u8; 12]);
            for point in triangle {
                for value in point {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            bytes.extend_from_slice(&[0u8; 2]);
        }
        bytes
    }

    fn collect(stl: &Stl) -> Vec<Triangle> {
        let mut all = Vec::new();
        stl.each(&mut |t| all.push(*t));
        all
    }

    const ONE: Triangle = [[0., 0., 0.], [1., 0., 0.], [0., 1., 0.5]];

    #[test]
    fn binary_is_read_by_its_length() {
        let bytes = binary(b"exported", &[ONE, ONE]);
        let stl = parse(&bytes).unwrap();
        assert!(matches!(stl, Stl::Binary(_)));
        assert_eq!(collect(&stl), vec![ONE, ONE]);
    }

    #[test]
    fn a_binary_header_that_says_solid_is_still_binary() {
        let bytes = binary(b"solid part exported by CAD", &[ONE]);
        let stl = parse(&bytes).unwrap();
        assert!(matches!(stl, Stl::Binary(_)));
        assert_eq!(collect(&stl), vec![ONE]);
    }

    #[test]
    fn ascii_is_parsed() {
        let text = "solid t\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   \
                    vertex 1 0 0\n   vertex 0 1 0.5\n  endloop\n endfacet\nendsolid t\n";
        let stl = parse(text.as_bytes()).unwrap();
        assert!(matches!(stl, Stl::Ascii(_)));
        assert_eq!(collect(&stl), vec![ONE]);
    }

    #[test]
    fn other_files_are_refused() {
        assert!(parse(b"hello").is_err());
        assert!(parse(b"solid but nothing in it\nendsolid\n").is_err());
    }
}
