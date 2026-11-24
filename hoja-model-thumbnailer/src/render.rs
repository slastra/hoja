//! A mesh drawn on the CPU: a z-buffer, flat shading, and 2×2 samples a
//! pixel for smooth edges.
//!
//! The view is the one a slicer opens with: Z up, from the front right and
//! above, with no perspective. The picture is cut to the model's outline plus
//! a margin, so a tall model gives a tall picture and a file manager can give
//! it a card of that shape.

use crate::Error;

pub type Point = [f32; 3];
pub type Triangle = [Point; 3];

/// A source of triangles that can be walked more than once.
pub trait Mesh {
    fn each(&self, f: &mut dyn FnMut(&Triangle));
}

impl Mesh for Vec<Triangle> {
    fn each(&self, f: &mut dyn FnMut(&Triangle)) {
        self.iter().for_each(f);
    }
}

/// Space around the model, as a part of the picture's long side.
const MARGIN: f32 = 0.06;
/// Samples a pixel along each axis.
const SAMPLES: usize = 2;
/// The view, in degrees: around Z from the front (-Y), and up from the bed.
const AZIMUTH: f32 = -45.;
const ELEVATION: f32 = 30.;
/// The model's color, in sRGB. A light gray with some blue in it, so that its
/// lit faces stand out on a dark theme and its shaded faces on a light one.
const COLOR: [f32; 3] = [0.74, 0.77, 0.82];
/// How much light a face gets with no light on it, and from the key light.
const AMBIENT: f32 = 0.22;
const KEY: f32 = 0.68;
/// A weak light from the camera, so that no face seen is black.
const FILL: f32 = 0.16;

/// The camera's frame: right and up on the picture, and toward the viewer.
struct View {
    right: Point,
    up: Point,
    toward: Point,
    light: Point,
}

impl View {
    fn new() -> View {
        let (az, el) = (AZIMUTH.to_radians(), ELEVATION.to_radians());
        // From the model toward the camera. Azimuth 0 is the front, -Y.
        let toward = [-(el.cos() * az.sin()), -el.cos() * az.cos(), el.sin()];
        let toward = normalize(toward);
        let right = normalize(cross([-toward[0], -toward[1], -toward[2]], [0., 0., 1.]));
        let up = cross(right, [-toward[0], -toward[1], -toward[2]]);
        // From the upper left, a little in front.
        let light = normalize(add(add(scale(right, -0.45), scale(up, 0.6)), scale(toward, 0.65)));
        View {
            right,
            up,
            toward,
            light,
        }
    }
}

pub fn render(mesh: &dyn Mesh, size: u32) -> Result<image::RgbaImage, Error> {
    let view = View::new();

    // First pass: the model's outline on the picture plane.
    let mut count = 0usize;
    let (mut min, mut max) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
    mesh.each(&mut |triangle| {
        count += 1;
        for point in triangle {
            let at = [dot(*point, view.right), dot(*point, view.up)];
            for axis in 0..2 {
                min[axis] = min[axis].min(at[axis]);
                max[axis] = max[axis].max(at[axis]);
            }
        }
    });
    if count == 0 {
        return Err("the model has no triangles".into());
    }
    let extent = [max[0] - min[0], max[1] - min[1]];
    if !extent.iter().all(|e| e.is_finite()) || extent[0].max(extent[1]) <= 0. {
        return Err("the model has no size".into());
    }

    // The picture: the long side is `size`, the margin is on every side.
    let size = size as f32;
    let margin = (MARGIN * size).round();
    let fit = (size - 2. * margin) / extent[0].max(extent[1]);
    let width = ((extent[0] * fit + 2. * margin).round() as usize).clamp(1, size as usize);
    let height = ((extent[1] * fit + 2. * margin).round() as usize).clamp(1, size as usize);
    // Centered in the picture, whatever rounding took.
    let offset = [
        (width as f32 - extent[0] * fit) / 2.,
        (height as f32 - extent[1] * fit) / 2.,
    ];

    let (sw, sh) = (width * SAMPLES, height * SAMPLES);
    let ss = SAMPLES as f32;
    let mut depth = vec![f32::NEG_INFINITY; sw * sh];
    let mut light = vec![0f32; sw * sh];

    // Second pass: every triangle into the samples.
    mesh.each(&mut |triangle| {
        let normal = cross(sub(triangle[1], triangle[0]), sub(triangle[2], triangle[0]));
        let length = dot(normal, normal).sqrt();
        // NaN too: a triangle with a corner at infinity.
        if !length.is_finite() || length <= 0. {
            return;
        }
        let mut normal = scale(normal, 1. / length);
        // Both sides are lit as the front. A mesh's winding is often wrong,
        // and an open mesh shows its inside.
        if dot(normal, view.toward) < 0. {
            normal = scale(normal, -1.);
        }
        let shade = AMBIENT
            + KEY * dot(normal, view.light).max(0.)
            + FILL * dot(normal, view.toward).max(0.);

        // Samples, with y down, and depth growing toward the viewer.
        let corner = |p: Point| {
            [
                ((dot(p, view.right) - min[0]) * fit + offset[0]) * ss,
                ((max[1] - dot(p, view.up)) * fit + offset[1]) * ss,
                dot(p, view.toward),
            ]
        };
        let [a, b, c] = [corner(triangle[0]), corner(triangle[1]), corner(triangle[2])];
        let area = edge(a, b, c);
        if area.abs() < 1e-12 {
            return;
        }
        let x0 = a[0].min(b[0]).min(c[0]).floor().max(0.) as usize;
        let y0 = a[1].min(b[1]).min(c[1]).floor().max(0.) as usize;
        let x1 = (a[0].max(b[0]).max(c[0]).ceil() as usize).min(sw);
        let y1 = (a[1].max(b[1]).max(c[1]).ceil() as usize).min(sh);
        for y in y0..y1 {
            for x in x0..x1 {
                let p = [x as f32 + 0.5, y as f32 + 0.5, 0.];
                let (w0, w1, w2) = (edge(b, c, p), edge(c, a, p), edge(a, b, p));
                let inside = if area > 0. {
                    w0 >= 0. && w1 >= 0. && w2 >= 0.
                } else {
                    w0 <= 0. && w1 <= 0. && w2 <= 0.
                };
                if !inside {
                    continue;
                }
                let z = (w0 * a[2] + w1 * b[2] + w2 * c[2]) / area;
                let i = y * sw + x;
                if z > depth[i] {
                    depth[i] = z;
                    light[i] = shade;
                }
            }
        }
    });

    // Down to pixels: the color of the samples that the model covers, and
    // the part it covers as alpha.
    let color = COLOR.map(to_linear);
    let mut picture = image::RgbaImage::new(width as u32, height as u32);
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        let (mut covered, mut sum) = (0usize, [0f32; 3]);
        for sy in 0..SAMPLES {
            for sx in 0..SAMPLES {
                let i = (y as usize * SAMPLES + sy) * sw + x as usize * SAMPLES + sx;
                if depth[i] > f32::NEG_INFINITY {
                    covered += 1;
                    for channel in 0..3 {
                        sum[channel] += to_srgb(color[channel] * light[i]);
                    }
                }
            }
        }
        if covered > 0 {
            let n = covered as f32;
            let byte = |v: f32| (v * 255.).round().clamp(0., 255.) as u8;
            *pixel = image::Rgba([
                byte(sum[0] / n),
                byte(sum[1] / n),
                byte(sum[2] / n),
                byte(n / (SAMPLES * SAMPLES) as f32),
            ]);
        }
    }
    Ok(picture)
}

/// Twice the signed area of `a b p`, on the picture plane.
fn edge(a: Point, b: Point, p: Point) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn to_srgb(v: f32) -> f32 {
    let v = v.clamp(0., 1.);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1. / 2.4) - 0.055
    }
}

fn add(a: Point, b: Point) -> Point {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: Point, b: Point) -> Point {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: Point, k: f32) -> Point {
    [a[0] * k, a[1] * k, a[2] * k]
}

fn dot(a: Point, b: Point) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: Point, b: Point) -> Point {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: Point) -> Point {
    scale(a, 1. / dot(a, a).sqrt())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A box from `lo` to `hi`, twelve triangles, wound outward.
    pub(crate) fn cuboid(lo: Point, hi: Point) -> Vec<Triangle> {
        let p = |x: usize, y: usize, z: usize| {
            [
                if x == 0 { lo[0] } else { hi[0] },
                if y == 0 { lo[1] } else { hi[1] },
                if z == 0 { lo[2] } else { hi[2] },
            ]
        };
        let quads = [
            [p(0, 0, 0), p(0, 1, 0), p(1, 1, 0), p(1, 0, 0)],
            [p(0, 0, 1), p(1, 0, 1), p(1, 1, 1), p(0, 1, 1)],
            [p(0, 0, 0), p(1, 0, 0), p(1, 0, 1), p(0, 0, 1)],
            [p(0, 1, 0), p(0, 1, 1), p(1, 1, 1), p(1, 1, 0)],
            [p(0, 0, 0), p(0, 0, 1), p(0, 1, 1), p(0, 1, 0)],
            [p(1, 0, 0), p(1, 1, 0), p(1, 1, 1), p(1, 0, 1)],
        ];
        quads
            .iter()
            .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
            .collect()
    }

    fn alpha(picture: &image::RgbaImage, x: u32, y: u32) -> u8 {
        picture.get_pixel(x, y)[3]
    }

    #[test]
    fn a_cube_fills_the_middle_and_leaves_the_corners_clear() {
        let picture = render(&cuboid([0.; 3], [10.; 3]), 256).unwrap();
        let (w, h) = picture.dimensions();
        assert!(w <= 256 && h <= 256 && w.max(h) == 256, "{w}×{h}");
        assert_eq!(alpha(&picture, w / 2, h / 2), 255);
        assert_eq!(alpha(&picture, 0, 0), 0);
        assert_eq!(alpha(&picture, w - 1, h - 1), 0);
        // Three faces seen, three shades.
        let mut shades: Vec<_> = picture
            .pixels()
            .filter(|p| p[3] == 255)
            .map(|p| p[0])
            .collect();
        shades.sort_unstable();
        shades.dedup();
        assert!(shades.len() >= 3, "{shades:?}");
    }

    #[test]
    fn a_tall_model_gives_a_tall_picture() {
        let picture = render(&cuboid([0.; 3], [5., 5., 40.]), 256).unwrap();
        let (w, h) = picture.dimensions();
        assert_eq!(h, 256);
        assert!(w < h / 2, "{w}×{h}");
    }

    #[test]
    fn a_mesh_wound_inward_is_lit_the_same() {
        let out = render(&cuboid([0.; 3], [10.; 3]), 64).unwrap();
        let inward: Vec<Triangle> = cuboid([0.; 3], [10.; 3])
            .into_iter()
            .map(|[a, b, c]| [a, c, b])
            .collect();
        assert_eq!(render(&inward, 64).unwrap(), out);
    }

    #[test]
    fn an_empty_or_flat_model_is_refused() {
        assert!(render(&Vec::<Triangle>::new(), 64).is_err());
        let point = [[1., 1., 1.]; 3];
        assert!(render(&vec![point], 64).is_err());
    }
}
