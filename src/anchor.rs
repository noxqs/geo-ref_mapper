//! Working out where a map sits on the earth from points marked by hand.
//!
//! A scanned map carries no position. What it does have is places you can
//! recognise: a river mouth, a crossroads, a coastline. Mark one on the basemap
//! and the same one on the picture and you have a pair. Enough pairs and there
//! is exactly one sensible answer to "where does this map go", and this is
//! where it is worked out.
//!
//! The answer is a **similarity**: turn, scale, move. Nothing else. A scan can
//! be rotated on the glass and photographed at any size, but it cannot be
//! stretched along one axis without the map itself being wrong — so allowing
//! that would let a bad set of points produce a confident, skewed placement.
//! Refusing it means a bad point shows up as a large error instead of being
//! quietly absorbed.
//!
//! Solved in one step rather than by searching. For a similarity the
//! best-fitting turn, scale and move have closed forms, so there is no starting
//! guess to get wrong and no iteration to fail to converge.
//!
//! **Down the picture is north-ish; up the world is north.** Picture rows are
//! numbered downwards and the earth's coordinates go up, so there is always a
//! flip between the two. It is *not* part of what is being solved — a scan is
//! not mirrored — so it is applied before the fitting and folded back in
//! afterwards. Leaving it in the fitting is worse than wrong: a rotation cannot
//! express a flip, so the solver quietly returns a turn of roughly half a circle
//! and a size that fits nothing.

/// One place, marked on both sides — the basemap and the picture.
#[derive(Clone, Debug, PartialEq)]
pub struct Pair {
    pub id: i64,
    pub name: String,
    /// Where it is on the earth, in Web Mercator metres — the same numbers
    /// GeoTIFF stores, so the answer can be written straight into the file.
    pub world: (f64, f64),
    /// Where it is on the picture, in full-size pixels.
    pub pixel: (f64, f64),
}

/// Turn, scale and move that takes picture pixels to the earth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Ground metres per pixel.
    pub scale: f64,
    /// Turn, anticlockwise, in radians.
    pub rotation: f64,
    /// Where pixel (0,0) lands.
    pub origin: (f64, f64),
}

/// Picture pixels with the row direction turned round, so both sides of the
/// fitting count the same way up.
fn upright(pixel: (f64, f64)) -> (f64, f64) {
    (pixel.0, -pixel.1)
}

impl Placement {
    /// Where a pixel lands on the earth.
    pub fn apply(&self, pixel: (f64, f64)) -> (f64, f64) {
        let (sin, cos) = self.rotation.sin_cos();
        let (px, py) = upright(pixel);
        (
            self.origin.0 + self.scale * (cos * px - sin * py),
            self.origin.1 + self.scale * (sin * px + cos * py),
        )
    }

    /// Which pixel of the picture a place on the earth falls on.
    ///
    /// The way back through [`apply`](Self::apply): take the move out, turn the
    /// other way, divide by the scale, and put the row direction back. Wanted
    /// whenever the question runs from the earth towards the picture — keeping
    /// the two halves of the placing view over the same ground, or saying where
    /// on a scan a marked place ought to be.
    ///
    /// The answer is not bounded to the picture. A place off the edge of the
    /// scan gives a pixel off the edge of it, which is the honest answer.
    pub fn pixel_of(&self, world: (f64, f64)) -> (f64, f64) {
        let (sin, cos) = self.rotation.sin_cos();
        let (dx, dy) = (
            (world.0 - self.origin.0) / self.scale,
            (world.1 - self.origin.1) / self.scale,
        );
        // Turning back is the same rotation with the angle negated.
        upright((cos * dx + sin * dy, -sin * dx + cos * dy))
    }

    /// The turn in degrees, for saying out loud.
    pub fn rotation_degrees(&self) -> f64 {
        self.rotation.to_degrees()
    }

    /// The six numbers GeoTIFF's `ModelTransformation` wants, as a 4×4 matrix
    /// in row order.
    ///
    /// Written rather than the simpler tiepoint-and-scale pair whenever there
    /// is any turn at all: the simple form cannot express one, and a placement
    /// stored without its rotation is silently in the wrong place.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn transformation_matrix(&self) -> [f64; 16] {
        let (sin, cos) = self.rotation.sin_cos();
        // A step across the picture and a step *down* it, each in world terms.
        // The second column carries the flip, which is why its signs do not
        // look like a plain rotation.
        let (a, b) = (self.scale * cos, self.scale * sin);
        let (d, e) = (self.scale * sin, -self.scale * cos);
        [
            a,
            b,
            0.0,
            self.origin.0,
            d,
            e,
            0.0,
            self.origin.1,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
        ]
    }
}

/// How well one anchor was matched.
#[derive(Clone, Debug, PartialEq)]
pub struct Residual {
    pub id: i64,
    /// How far the placement puts it from where it was said to be, in metres.
    pub metres: f64,
}

/// What the anchors add up to.
#[derive(Clone, Debug, PartialEq)]
pub struct Fit {
    pub placement: Placement,
    pub residuals: Vec<Residual>,
    /// Typical error across all of them, in metres.
    pub average_metres: f64,
    /// The worst single one.
    pub worst_metres: f64,
}

/// Why a set of anchors cannot be solved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Fewer than two: one point fixes a position but not a turn or a scale.
    TooFew,
    /// Every point in the same spot on the picture, or on the earth. There is
    /// no scale to work out from a set with no extent.
    NoSpread,
}

impl Refused {
    pub fn why(self) -> &'static str {
        match self {
            Self::TooFew => {
                "at least two anchors are needed — one fixes a spot but not the turn or the size"
            }
            Self::NoSpread => {
                "the anchors are all in the same place, so there is no size to work out"
            }
        }
    }
}

/// Solves for the turn, scale and move that best fit the anchors.
///
/// Two anchors give an exact answer. More than two are over-determined, so the
/// answer is the one with the smallest total error — and the leftovers are what
/// the residuals report.
pub fn solve(anchors: &[Pair]) -> Result<Fit, Refused> {
    if anchors.len() < 2 {
        return Err(Refused::TooFew);
    }

    let count = anchors.len() as f64;
    let pixel_centre = (
        anchors.iter().map(|a| upright(a.pixel).0).sum::<f64>() / count,
        anchors.iter().map(|a| upright(a.pixel).1).sum::<f64>() / count,
    );
    let world_centre = (
        anchors.iter().map(|a| a.world.0).sum::<f64>() / count,
        anchors.iter().map(|a| a.world.1).sum::<f64>() / count,
    );

    // Measured from the middle of each set, which is what lets the turn and the
    // scale be found without the move getting in the way.
    let mut spread = 0.0;
    let mut along = 0.0;
    let mut across = 0.0;
    for anchor in anchors {
        let upright = upright(anchor.pixel);
        let (px, py) = (upright.0 - pixel_centre.0, upright.1 - pixel_centre.1);
        let (wx, wy) = (
            anchor.world.0 - world_centre.0,
            anchor.world.1 - world_centre.1,
        );
        spread += px * px + py * py;
        along += px * wx + py * wy;
        across += px * wy - py * wx;
    }
    if spread <= f64::EPSILON {
        return Err(Refused::NoSpread);
    }

    let rotation = across.atan2(along);
    let scale = (along * along + across * across).sqrt() / spread;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(Refused::NoSpread);
    }

    // The move is whatever is left once the turn and scale are taken out.
    let (sin, cos) = rotation.sin_cos();
    let placement = Placement {
        scale,
        rotation,
        origin: (
            world_centre.0 - scale * (cos * pixel_centre.0 - sin * pixel_centre.1),
            world_centre.1 - scale * (sin * pixel_centre.0 + cos * pixel_centre.1),
        ),
    };

    let residuals: Vec<Residual> = anchors
        .iter()
        .map(|anchor| {
            let (x, y) = placement.apply(anchor.pixel);
            let (dx, dy) = (x - anchor.world.0, y - anchor.world.1);
            Residual {
                id: anchor.id,
                metres: (dx * dx + dy * dy).sqrt(),
            }
        })
        .collect();

    let average_metres = residuals.iter().map(|r| r.metres).sum::<f64>() / count;
    let worst_metres = residuals.iter().map(|r| r.metres).fold(0.0_f64, f64::max);

    Ok(Fit {
        placement,
        residuals,
        average_metres,
        worst_metres,
    })
}

/// How an error should read, from perfect to hopeless.
///
/// Judged against the size of the map rather than in bare metres: fifty metres
/// out on a map of Singapore is nothing, and on a plan of one street it is the
/// whole picture.
pub fn quality(metres: f64, map_extent_metres: f64) -> f32 {
    if map_extent_metres <= 0.0 {
        return 0.0;
    }
    let share = metres / map_extent_metres;
    // A thousandth of the map is as good as clicking gets; a fiftieth is wrong.
    let worst = 0.02;
    let best = 0.001;
    (1.0 - ((share - best) / (worst - best)).clamp(0.0, 1.0)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(id: i64, pixel: (f64, f64), world: (f64, f64)) -> Pair {
        Pair {
            id,
            name: format!("anchor {id}"),
            world,
            pixel,
        }
    }

    #[test]
    fn two_points_place_a_map_exactly() {
        // A picture whose pixels are two metres each, unturned, starting at
        // (1000, 2000).
        let anchors = vec![
            anchor(1, (0.0, 0.0), (1000.0, 2000.0)),
            anchor(2, (100.0, 0.0), (1200.0, 2000.0)),
        ];
        let fit = solve(&anchors).expect("two points are enough");
        assert!((fit.placement.scale - 2.0).abs() < 1e-9);
        assert!(fit.placement.rotation.abs() < 1e-9);
        assert!((fit.placement.origin.0 - 1000.0).abs() < 1e-6);
        assert!(fit.worst_metres < 1e-6, "an exact fit must have no error");
    }

    #[test]
    fn going_to_the_earth_and_back_lands_on_the_same_pixel() {
        // Every turn, including ones that are not right angles — the flip and
        // the rotation are the two things a hand-written inverse gets wrong,
        // and both are silent when it happens.
        for rotation in [0.0, 0.4, -1.2, std::f64::consts::FRAC_PI_2, 3.0] {
            let placement = Placement {
                scale: 2.5,
                rotation,
                origin: (11_557_133.0, 151_086.0),
            };
            for pixel in [(0.0, 0.0), (4096.0, 2048.0), (-30.0, 12_000.0)] {
                let back = placement.pixel_of(placement.apply(pixel));
                assert!(
                    (back.0 - pixel.0).abs() < 1e-6 && (back.1 - pixel.1).abs() < 1e-6,
                    "turn {rotation}: {pixel:?} came back as {back:?}",
                );
            }
        }
    }

    #[test]
    fn going_down_the_picture_still_goes_south_on_the_way_back() {
        // The flip, from the other direction. A place *south* of where pixel
        // (0,0) lands must be further *down* the picture, not up it.
        let placement = Placement {
            scale: 1.0,
            rotation: 0.0,
            origin: (0.0, 0.0),
        };
        let south = placement.pixel_of((0.0, -500.0));
        assert!(
            south.1 > 0.0,
            "south of the top-left corner must be down the picture, got {south:?}",
        );
    }

    #[test]
    fn a_turned_scan_is_recognised_as_turned() {
        // The same map, rotated a quarter turn anticlockwise.
        let quarter = std::f64::consts::FRAC_PI_2;
        let anchors = vec![
            anchor(1, (0.0, 0.0), (0.0, 0.0)),
            anchor(2, (100.0, 0.0), (0.0, 100.0)),
        ];
        let fit = solve(&anchors).unwrap();
        assert!(
            (fit.placement.rotation - quarter).abs() < 1e-9,
            "came out at {} degrees",
            fit.placement.rotation_degrees(),
        );
        assert!((fit.placement.scale - 1.0).abs() < 1e-9);
    }

    #[test]
    fn one_point_is_refused_rather_than_guessed_at() {
        let anchors = vec![anchor(1, (0.0, 0.0), (1000.0, 2000.0))];
        assert_eq!(solve(&anchors), Err(Refused::TooFew));
    }

    #[test]
    fn points_all_in_one_spot_are_refused() {
        let anchors = vec![
            anchor(1, (50.0, 50.0), (0.0, 0.0)),
            anchor(2, (50.0, 50.0), (100.0, 100.0)),
        ];
        assert_eq!(solve(&anchors), Err(Refused::NoSpread));
    }

    #[test]
    fn a_misplaced_anchor_shows_up_as_its_own_error() {
        // Three good points and one clicked in the wrong place. The bad one
        // must be the one the table blames — absorbing it into the others is
        // exactly what a stretchier model would do.
        // Rows run down and the world runs up, so a point a hundred rows lower
        // is a hundred metres further *south*. Three that agree, and one put
        // forty metres out.
        let anchors = vec![
            anchor(1, (0.0, 0.0), (0.0, 0.0)),
            anchor(2, (100.0, 0.0), (100.0, 0.0)),
            anchor(3, (0.0, 100.0), (0.0, -100.0)),
            anchor(4, (100.0, 100.0), (140.0, -100.0)),
        ];
        let fit = solve(&anchors).unwrap();
        let worst = fit
            .residuals
            .iter()
            .max_by(|a, b| a.metres.total_cmp(&b.metres))
            .unwrap();
        assert_eq!(worst.id, 4, "the wrongly placed point must be named");
        assert!(worst.metres > 10.0, "and its error must be obvious");
    }

    #[test]
    fn more_points_than_needed_still_fit_when_they_agree() {
        // Going right and down the picture is going east and south.
        let anchors: Vec<Pair> = (0..6)
            .map(|index| {
                let p = index as f64 * 40.0;
                anchor(index, (p, p * 0.5), (500.0 + p * 3.0, 700.0 - p * 1.5))
            })
            .collect();
        let fit = solve(&anchors).unwrap();
        assert!(
            fit.worst_metres < 1e-6,
            "points that agree must fit exactly, worst was {}",
            fit.worst_metres,
        );
        assert!((fit.placement.scale - 3.0).abs() < 1e-9);
    }

    #[test]
    fn the_placement_puts_the_corner_where_it_belongs() {
        let anchors = vec![
            anchor(1, (10.0, 10.0), (1010.0, 2010.0)),
            anchor(2, (110.0, 10.0), (1110.0, 2010.0)),
        ];
        let fit = solve(&anchors).unwrap();
        let corner = fit.placement.apply((0.0, 0.0));
        assert!((corner.0 - 1000.0).abs() < 1e-6);
        // Pixel row 10 sits at 2010, and rows run *down* while the world runs
        // up — so row 0 is ten metres further north, not ten less.
        assert!(
            (corner.1 - 2020.0).abs() < 1e-6,
            "the corner came out at {}",
            corner.1,
        );
    }

    #[test]
    fn the_matrix_agrees_with_the_placement_it_came_from() {
        let fit = solve(&[
            anchor(1, (0.0, 0.0), (5000.0, 9000.0)),
            anchor(2, (200.0, 100.0), (5600.0, 9300.0)),
        ])
        .unwrap();
        let matrix = fit.placement.transformation_matrix();

        // Row one and row two are the turn and scale; the fourth column is the
        // move. Applying them by hand must land where `apply` does.
        let pixel = (137.0, 42.0);
        let by_matrix = (
            matrix[0] * pixel.0 + matrix[1] * pixel.1 + matrix[3],
            matrix[4] * pixel.0 + matrix[5] * pixel.1 + matrix[7],
        );
        let by_placement = fit.placement.apply(pixel);
        assert!(
            (by_matrix.0 - by_placement.0).abs() < 1e-6,
            "{by_matrix:?} against {by_placement:?}",
        );
    }

    #[test]
    fn a_plain_north_up_map_comes_out_unturned() {
        // The everyday case, and the one that was wrong: rows running down and
        // the world running up is a flip, not a turn. Fitting it with a turn
        // gave roughly half a circle and a size that matched nothing.
        let anchors = vec![
            anchor(1, (0.0, 0.0), (1000.0, 5000.0)),
            anchor(2, (100.0, 0.0), (1200.0, 5000.0)),
            anchor(3, (0.0, 50.0), (1000.0, 4900.0)),
        ];
        let fit = solve(&anchors).unwrap();
        assert!(
            fit.placement.rotation.abs() < 1e-9,
            "came out turned by {} degrees",
            fit.placement.rotation_degrees(),
        );
        assert!((fit.placement.scale - 2.0).abs() < 1e-9);
        assert!(fit.worst_metres < 1e-6, "and it must fit exactly");
    }

    #[test]
    fn going_down_the_picture_goes_south() {
        let fit = solve(&[
            anchor(1, (0.0, 0.0), (0.0, 0.0)),
            anchor(2, (100.0, 0.0), (100.0, 0.0)),
        ])
        .unwrap();
        let lower = fit.placement.apply((0.0, 100.0));
        assert!(
            lower.1 < 0.0,
            "a hundred rows down must be south, not north — got {}",
            lower.1,
        );
    }

    #[test]
    fn quality_reads_from_perfect_to_hopeless() {
        let map = 12_000.0; // a map twelve kilometres across
        assert!(
            quality(5.0, map) > 0.9,
            "five metres out is as good as it gets"
        );
        assert!(quality(300.0, map) < 0.1, "three hundred is hopeless");
        // And the same error on a small map is worse.
        assert!(quality(50.0, 500.0) < quality(50.0, map));
    }
}
