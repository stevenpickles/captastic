//! A small antialiased painter for icons, drawn straight into a 32-bit DIB.
//!
//! GDI's pens and brushes alias every diagonal and curve, which is tolerable for the overlay's
//! square glyphs and not for a pen nib or a colour swatch. Everything here works in fractional
//! pixel coordinates and blends by coverage: circles and capsules analytically, polygons by
//! sampling each pixel sixteen times. Nothing reads or writes GDI state, so it is plain arithmetic
//! over a byte slice and is tested as such; a caller flushes GDI before handing the slice over.

/// A blue, green, red colour, in the DIB's own byte order.
pub(super) type Bgr = [u8; 3];

pub(super) const fn bgr(red: u8, green: u8, blue: u8) -> Bgr {
    [blue, green, red]
}

/// Paints into a tightly packed, top-down BGRA buffer `width` pixels wide.
pub(super) struct Painter<'a> {
    pixels: &'a mut [u8],
    width: usize,
    height: usize,
}

impl<'a> Painter<'a> {
    pub(super) fn new(pixels: &'a mut [u8], width: usize) -> Self {
        let height = pixels.len() / (width * 4).max(1);
        Self {
            pixels,
            width,
            height,
        }
    }

    /// A filled disc.
    pub(super) fn circle(&mut self, centre: (f32, f32), radius: f32, colour: Bgr, opacity: f32) {
        self.cover(
            (centre.0 - radius, centre.1 - radius),
            (centre.0 + radius, centre.1 + radius),
            colour,
            opacity,
            |x, y| (radius + 0.5 - distance((x, y), centre)).clamp(0.0, 1.0),
        );
    }

    /// A ring `width` pixels wide whose outer edge is `radius` from `centre`.
    pub(super) fn ring(
        &mut self,
        centre: (f32, f32),
        radius: f32,
        width: f32,
        colour: Bgr,
        opacity: f32,
    ) {
        self.cover(
            (centre.0 - radius, centre.1 - radius),
            (centre.0 + radius, centre.1 + radius),
            colour,
            opacity,
            |x, y| {
                let from_centre = distance((x, y), centre);
                let outer = (radius + 0.5 - from_centre).clamp(0.0, 1.0);
                let inner = (from_centre - (radius - width) + 0.5).clamp(0.0, 1.0);
                outer.min(inner)
            },
        );
    }

    /// A line from `start` to `end` with round ends, `radius` pixels either side of it.
    pub(super) fn capsule(
        &mut self,
        start: (f32, f32),
        end: (f32, f32),
        radius: f32,
        colour: Bgr,
        opacity: f32,
    ) {
        self.cover(
            (start.0.min(end.0) - radius, start.1.min(end.1) - radius),
            (start.0.max(end.0) + radius, start.1.max(end.1) + radius),
            colour,
            opacity,
            |x, y| (radius + 0.5 - distance_to_segment((x, y), start, end)).clamp(0.0, 1.0),
        );
    }

    /// A filled polygon, by the even-odd rule, its edges sampled four by four in each pixel.
    pub(super) fn polygon(&mut self, points: &[(f32, f32)], colour: Bgr, opacity: f32) {
        if points.len() < 3 {
            return;
        }
        let (mut low, mut high) = ((f32::MAX, f32::MAX), (f32::MIN, f32::MIN));
        for &(x, y) in points {
            low = (low.0.min(x), low.1.min(y));
            high = (high.0.max(x), high.1.max(y));
        }
        self.cover(low, high, colour, opacity, |x, y| {
            let mut inside = 0_u32;
            for row in 0..4 {
                for column in 0..4 {
                    let sample = (
                        x - 0.5 + (column as f32 + 0.5) / 4.0,
                        y - 0.5 + (row as f32 + 0.5) / 4.0,
                    );
                    if contains(points, sample) {
                        inside += 1;
                    }
                }
            }
            inside as f32 / 16.0
        });
    }

    /// Blends `colour` over every pixel of the box from `low` to `high` by the coverage `at`
    /// gives that pixel's centre.
    fn cover(
        &mut self,
        low: (f32, f32),
        high: (f32, f32),
        colour: Bgr,
        opacity: f32,
        at: impl Fn(f32, f32) -> f32,
    ) {
        let clamp = |value: f32, limit: usize| value.max(0.0).min(limit as f32) as usize;
        let (left, top) = (
            clamp(low.0.floor() - 1.0, self.width),
            clamp(low.1.floor() - 1.0, self.height),
        );
        let (right, bottom) = (
            clamp(high.0.ceil() + 1.0, self.width),
            clamp(high.1.ceil() + 1.0, self.height),
        );
        for y in top..bottom {
            for x in left..right {
                let cover = at(x as f32 + 0.5, y as f32 + 0.5) * opacity;
                if cover <= 0.0 {
                    continue;
                }
                let index = (y * self.width + x) * 4;
                for (target, above) in self.pixels[index..index + 3].iter_mut().zip(colour) {
                    let below = f32::from(*target);
                    *target = (below + (f32::from(above) - below) * cover.min(1.0)).round() as u8;
                }
            }
        }
    }
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn distance_to_segment(point: (f32, f32), start: (f32, f32), end: (f32, f32)) -> f32 {
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let length_squared = dx * dx + dy * dy;
    let along = if length_squared > 0.0 {
        (((point.0 - start.0) * dx + (point.1 - start.1) * dy) / length_squared).clamp(0.0, 1.0)
    } else {
        0.0
    };
    distance(point, (start.0 + along * dx, start.1 + along * dy))
}

/// Whether `point` is inside `polygon`, by the even-odd rule.
fn contains(polygon: &[(f32, f32)], point: (f32, f32)) -> bool {
    let mut inside = false;
    let mut previous = polygon[polygon.len() - 1];
    for &current in polygon {
        if (current.1 > point.1) != (previous.1 > point.1) {
            let crossing = current.0
                + (point.1 - current.1) * (previous.0 - current.0) / (previous.1 - current.1);
            if point.0 < crossing {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

/// Places an icon drawn on a diagonal: `t` runs from the lower-left start toward the upper right,
/// as a share of the run, and `u` runs across it, as a share of the icon's size, positive toward
/// the lower right.
#[derive(Clone, Copy)]
pub(super) struct Diagonal {
    start: (f32, f32),
    along: (f32, f32),
    across: (f32, f32),
}

impl Diagonal {
    /// The diagonal of the square at `(left, top)` of side `size`, inset by `inset` of the size at
    /// each end.
    pub(super) fn new(left: f32, top: f32, size: f32, inset: f32) -> Self {
        let start = (left + size * inset, top + size * (1.0 - inset));
        let end = (left + size * (1.0 - inset), top + size * inset);
        let along = (end.0 - start.0, end.1 - start.1);
        let length = (along.0 * along.0 + along.1 * along.1)
            .sqrt()
            .max(f32::EPSILON);
        Self {
            start,
            along,
            // Perpendicular to the run, scaled to the icon's size.
            across: (-along.1 / length * size, along.0 / length * size),
        }
    }

    pub(super) fn at(&self, t: f32, u: f32) -> (f32, f32) {
        (
            self.start.0 + self.along.0 * t + self.across.0 * u,
            self.start.1 + self.along.1 * t + self.across.1 * u,
        )
    }

    pub(super) fn quad(&self, corners: [(f32, f32); 4]) -> [(f32, f32); 4] {
        corners.map(|(t, u)| self.at(t, u))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(width: usize, height: usize) -> Vec<u8> {
        vec![0_u8; width * height * 4]
    }

    fn blue(pixels: &[u8], width: usize, x: usize, y: usize) -> u8 {
        pixels[(y * width + x) * 4]
    }

    #[test]
    fn a_disc_is_solid_inside_clear_outside_and_soft_at_its_edge() {
        let mut pixels = canvas(20, 20);
        Painter::new(&mut pixels, 20).circle((10.0, 10.0), 6.0, [200, 0, 0], 1.0);
        assert_eq!(blue(&pixels, 20, 10, 10), 200);
        assert_eq!(blue(&pixels, 20, 0, 0), 0);
        // Somewhere on the rim a pixel is part-covered: the edge is antialiased.
        assert!((0..20).any(|x| {
            let value = blue(&pixels, 20, x, 10);
            value > 0 && value < 200
        }));
    }

    #[test]
    fn a_ring_leaves_its_middle_untouched() {
        let mut pixels = canvas(20, 20);
        Painter::new(&mut pixels, 20).ring((10.0, 10.0), 8.0, 2.0, [200, 0, 0], 1.0);
        assert_eq!(blue(&pixels, 20, 10, 10), 0);
        assert_eq!(blue(&pixels, 20, 3, 10), 200);
    }

    #[test]
    fn a_polygon_fills_by_coverage_and_respects_opacity() {
        let mut pixels = canvas(10, 10);
        let square = [(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0)];
        Painter::new(&mut pixels, 10).polygon(&square, [200, 0, 0], 0.5);
        assert_eq!(blue(&pixels, 10, 5, 5), 100);
        assert_eq!(blue(&pixels, 10, 0, 5), 0);
        // A pixel the edge halves is covered by half of half.
        let mut pixels = canvas(10, 10);
        let half = [(2.0, 2.0), (5.5, 2.0), (5.5, 8.0), (2.0, 8.0)];
        Painter::new(&mut pixels, 10).polygon(&half, [200, 0, 0], 1.0);
        assert_eq!(blue(&pixels, 10, 5, 5), 100);
    }

    #[test]
    fn painting_off_the_canvas_is_clipped_not_a_panic() {
        let mut pixels = canvas(4, 4);
        let mut painter = Painter::new(&mut pixels, 4);
        painter.circle((-10.0, -10.0), 30.0, [1, 2, 3], 1.0);
        painter.capsule((-5.0, 2.0), (50.0, 2.0), 3.0, [1, 2, 3], 1.0);
        assert_eq!(&pixels[0..3], &[1, 2, 3]);
    }

    #[test]
    fn the_diagonal_runs_from_lower_left_to_upper_right() {
        let diagonal = Diagonal::new(0.0, 0.0, 10.0, 0.0);
        assert_eq!(diagonal.at(0.0, 0.0), (0.0, 10.0));
        let (x, y) = diagonal.at(1.0, 0.0);
        assert!((x - 10.0).abs() < 1e-4 && y.abs() < 1e-4);
    }
}
