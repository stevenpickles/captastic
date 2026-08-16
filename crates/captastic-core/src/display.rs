use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub String);

impl DisplayId {
    /// A request-time alias for whichever display is currently primary.
    ///
    /// Enumerated physical displays should use a persistent platform identifier instead.
    pub fn primary() -> Self {
        Self("primary".to_owned())
    }

    /// The normalized, physical-pixel union of all attached desktop displays.
    pub fn virtual_desktop() -> Self {
        Self("virtual_desktop".to_owned())
    }

    pub fn is_primary_alias(&self) -> bool {
        self.0 == "primary"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    pub fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        let x = i64::from(x);
        let y = i64::from(y);
        x >= i64::from(self.x) && x < self.right() && y >= i64::from(self.y) && y < self.bottom()
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let left = i64::from(self.x).max(i64::from(other.x));
        let top = i64::from(self.y).max(i64::from(other.y));
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if left >= right || top >= bottom {
            return None;
        }
        Some(Self {
            x: i32::try_from(left).ok()?,
            y: i32::try_from(top).ok()?,
            width: u32::try_from(right - left).ok()?,
            height: u32::try_from(bottom - top).ok()?,
        })
    }

    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    fn union(self, other: Self) -> Option<Self> {
        let left = i64::from(self.x).min(i64::from(other.x));
        let top = i64::from(self.y).min(i64::from(other.y));
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Some(Self {
            x: i32::try_from(left).ok()?,
            y: i32::try_from(top).ok()?,
            width: u32::try_from(right - left).ok()?,
            height: u32::try_from(bottom - top).ok()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: String,
    pub bounds: Rect,
    pub scale_factor: f32,
    pub rotation_degrees: u16,
    pub is_primary: bool,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DisplayTopologyError {
    #[error("display topology contains an empty display identifier")]
    EmptyDisplayId,
    #[error("display topology contains duplicate display identifier {0}")]
    DuplicateDisplayId(String),
    #[error("display {0} has empty bounds")]
    EmptyDisplayBounds(String),
    #[error("display {display_id} has unsupported rotation {rotation_degrees}")]
    UnsupportedRotation {
        display_id: String,
        rotation_degrees: u16,
    },
    /// The offending value is carried as text rather than as an `f32`.
    ///
    /// NaN is the value this variant most often reports, and it is the one value that compares
    /// unequal to itself — an error holding it could never be matched against an expected error,
    /// which is the one thing a caller wants to do with it.
    #[error("display {display_id} has an unusable scale factor {scale_factor}")]
    InvalidScaleFactor {
        display_id: String,
        scale_factor: String,
    },
}

/// An immutable view of attached displays used to resolve one capture action.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayTopology {
    generation: u64,
    displays: Vec<DisplayInfo>,
}

impl DisplayTopology {
    pub fn new(generation: u64, displays: Vec<DisplayInfo>) -> Result<Self, DisplayTopologyError> {
        let mut ids = HashSet::with_capacity(displays.len());
        for display in &displays {
            if display.id.0.trim().is_empty() {
                return Err(DisplayTopologyError::EmptyDisplayId);
            }
            if !ids.insert(display.id.0.clone()) {
                return Err(DisplayTopologyError::DuplicateDisplayId(
                    display.id.0.clone(),
                ));
            }
            if display.bounds.width == 0 || display.bounds.height == 0 {
                return Err(DisplayTopologyError::EmptyDisplayBounds(
                    display.id.0.clone(),
                ));
            }
            if !matches!(display.rotation_degrees, 0 | 90 | 180 | 270) {
                return Err(DisplayTopologyError::UnsupportedRotation {
                    display_id: display.id.0.clone(),
                    rotation_degrees: display.rotation_degrees,
                });
            }
            // A topology is compared against its successor to decide whether anything changed, and
            // NaN compares unequal to itself — so one NaN scale factor makes every topology differ
            // from the one before it, including from itself, and the daemon sees a display change
            // that never happened. Zero is the other unusable value: it is what a DPI query that
            // succeeds while reporting nothing produces, and everything downstream divides by it.
            //
            // There is no upper bound here on purpose. Any finite positive factor is arithmetically
            // sound, and a ceiling invented today is a legitimate future display rejected later.
            if !display.scale_factor.is_finite() || display.scale_factor <= 0.0 {
                return Err(DisplayTopologyError::InvalidScaleFactor {
                    display_id: display.id.0.clone(),
                    scale_factor: display.scale_factor.to_string(),
                });
            }
        }
        Ok(Self {
            generation,
            displays,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn displays(&self) -> &[DisplayInfo] {
        &self.displays
    }

    pub fn primary(&self) -> Option<&DisplayInfo> {
        self.displays
            .iter()
            .find(|display| display.is_primary)
            .or_else(|| self.displays.first())
    }

    pub fn resolve(&self, id: &DisplayId) -> Option<&DisplayInfo> {
        if id.is_primary_alias() {
            self.primary()
        } else {
            self.displays.iter().find(|display| display.id == *id)
        }
    }

    pub fn containing_point(&self, x: i32, y: i32) -> Option<&DisplayInfo> {
        self.displays
            .iter()
            .filter(|display| display.bounds.contains(x, y))
            .min_by(|left, right| left.id.0.cmp(&right.id.0))
    }

    /// Resolves a spanning rectangle to exactly one display.
    ///
    /// The largest physical-pixel intersection wins. Stable display ID ordering resolves a tie.
    pub fn largest_intersection(&self, bounds: Rect) -> Option<&DisplayInfo> {
        self.displays
            .iter()
            .filter_map(|display| {
                display
                    .bounds
                    .intersection(bounds)
                    .map(|intersection| (display, intersection.area()))
            })
            .max_by(|(left, left_area), (right, right_area)| {
                left_area
                    .cmp(right_area)
                    .then_with(|| right.id.0.cmp(&left.id.0))
            })
            .map(|(display, _)| display)
    }

    pub fn virtual_bounds(&self) -> Option<Rect> {
        let mut bounds = self.displays.iter().map(|display| display.bounds);
        let first = bounds.next()?;
        bounds.try_fold(first, Rect::union)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, bounds: Rect, rotation_degrees: u16, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds,
            scale_factor: 1.0,
            rotation_degrees,
            is_primary: primary,
        }
    }

    fn mixed_topology() -> DisplayTopology {
        DisplayTopology::new(
            7,
            vec![
                display(
                    "left",
                    Rect {
                        x: -1920,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                    0,
                    false,
                ),
                display(
                    "primary-id",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 2560,
                        height: 1440,
                    },
                    0,
                    true,
                ),
                display(
                    "portrait",
                    Rect {
                        x: 2560,
                        y: -320,
                        width: 1080,
                        height: 1920,
                    },
                    90,
                    false,
                ),
            ],
        )
        .expect("valid topology")
    }

    #[test]
    fn resolves_primary_alias_and_pointer_with_negative_coordinates() {
        let topology = mixed_topology();
        assert_eq!(topology.generation(), 7);
        assert_eq!(
            topology
                .resolve(&DisplayId::primary())
                .map(|display| display.id.0.as_str()),
            Some("primary-id")
        );
        assert_eq!(
            topology
                .containing_point(-1, 500)
                .map(|display| display.id.0.as_str()),
            Some("left")
        );
        assert_eq!(
            topology
                .containing_point(3000, -100)
                .map(|display| display.id.0.as_str()),
            Some("portrait")
        );
        assert!(topology.containing_point(5000, 5000).is_none());
    }

    #[test]
    fn computes_virtual_bounds_across_resolution_and_origin_changes() {
        assert_eq!(
            mixed_topology().virtual_bounds(),
            Some(Rect {
                x: -1920,
                y: -320,
                width: 5560,
                height: 1920,
            })
        );
    }

    #[test]
    fn assigns_a_spanning_window_to_its_largest_intersection() {
        let topology = mixed_topology();
        let owner = topology
            .largest_intersection(Rect {
                x: -200,
                y: 100,
                width: 1000,
                height: 800,
            })
            .expect("window intersects a display");
        assert_eq!(owner.id.0, "primary-id");
    }

    #[test]
    fn intersection_ties_are_resolved_by_stable_id() {
        let topology = DisplayTopology::new(
            1,
            vec![
                display(
                    "b",
                    Rect {
                        x: 100,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                    0,
                    false,
                ),
                display(
                    "a",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                    0,
                    true,
                ),
            ],
        )
        .expect("valid topology");
        let owner = topology
            .largest_intersection(Rect {
                x: 50,
                y: 0,
                width: 100,
                height: 100,
            })
            .expect("window intersects both displays");
        assert_eq!(owner.id.0, "a");
    }

    #[test]
    fn rejects_duplicate_ids_and_unsupported_rotations() {
        let duplicate = display(
            "same",
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            0,
            true,
        );
        assert_eq!(
            DisplayTopology::new(1, vec![duplicate.clone(), duplicate]),
            Err(DisplayTopologyError::DuplicateDisplayId("same".to_owned()))
        );
        assert_eq!(
            DisplayTopology::new(
                1,
                vec![display(
                    "tilted",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                    45,
                    true,
                )],
            ),
            Err(DisplayTopologyError::UnsupportedRotation {
                display_id: "tilted".to_owned(),
                rotation_degrees: 45,
            })
        );
    }

    #[test]
    fn a_nan_scale_factor_makes_a_topology_differ_from_itself_and_is_rejected() {
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let mut unusable = display("only", bounds, 0, true);
        unusable.scale_factor = f32::NAN;

        // The hazard, before the rejection that exists because of it: topology change detection
        // compares the new topology against the old one, and this display is not equal to itself.
        // A daemon holding it would rebuild on every comparison, having observed no change at all.
        assert_ne!(unusable, unusable.clone());

        assert_eq!(
            DisplayTopology::new(1, vec![unusable]),
            Err(DisplayTopologyError::InvalidScaleFactor {
                display_id: "only".to_owned(),
                scale_factor: "NaN".to_owned(),
            })
        );
    }

    #[test]
    fn scale_factors_are_accepted_when_finite_and_positive() {
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let topology_with = |scale_factor: f32| {
            let mut only = display("only", bounds, 0, true);
            only.scale_factor = scale_factor;
            DisplayTopology::new(1, vec![only])
        };

        // Zero is what a DPI query that succeeds while reporting nothing produces, and everything
        // downstream divides by it.
        for rejected in [0.0, -1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            assert!(
                topology_with(rejected).is_err(),
                "scale factor {rejected} should be rejected"
            );
        }
        // No upper bound is imposed, so an unfamiliar high-DPI display is described rather than
        // refused. Every one of these is arithmetically sound.
        for accepted in [1.0, 1.25, 2.0, 3.5, 0.5, f32::MIN_POSITIVE, 1e30] {
            assert!(
                topology_with(accepted).is_ok(),
                "scale factor {accepted} should be accepted"
            );
        }
    }

    /// Properties of the desktop-coordinate algebra.
    ///
    /// The example tests above pin the layouts Captastic is actually run against. These cover the
    /// part those cannot: that the algebra holds for arbitrary layouts, and in particular that it
    /// holds left and above the origin. Every secondary display placed to the left of or above the
    /// primary has negative coordinates, so the negative half-plane is not an edge case here — it
    /// is half of every real virtual desktop, and Milestone 1 asks for it to survive every crop
    /// and overlay operation.
    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// Coordinates spanning both sides of the origin, sized like real desktops rather than
        /// like the integer types. A `Rect` at `i32::MIN` spanning `u32::MAX` is representable and
        /// no display can produce one; generating those would only measure how the arithmetic
        /// saturates, which is not what any of these properties are about.
        fn any_rect() -> impl Strategy<Value = Rect> {
            (
                -20_000i32..=20_000,
                -20_000i32..=20_000,
                1u32..=20_000,
                1u32..=20_000,
            )
                .prop_map(|(x, y, width, height)| Rect {
                    x,
                    y,
                    width,
                    height,
                })
        }

        fn any_point() -> impl Strategy<Value = (i32, i32)> {
            (-20_000i32..=20_000, -20_000i32..=20_000)
        }

        fn displays_from(bounds: Vec<Rect>) -> Vec<DisplayInfo> {
            bounds
                .into_iter()
                .enumerate()
                .map(|(index, bounds)| DisplayInfo {
                    id: DisplayId(format!("display-{index}")),
                    name: format!("display-{index}"),
                    bounds,
                    scale_factor: 1.0,
                    rotation_degrees: 0,
                    is_primary: index == 0,
                })
                .collect()
        }

        fn topology_of(displays: Vec<DisplayInfo>) -> DisplayTopology {
            DisplayTopology::new(1, displays)
                .expect("generated displays satisfy every topology invariant")
        }

        fn topology_from(bounds: Vec<Rect>) -> DisplayTopology {
            topology_of(displays_from(bounds))
        }

        fn any_display_bounds() -> impl Strategy<Value = Vec<Rect>> {
            prop::collection::vec(any_rect(), 1..=5)
        }

        proptest! {
            /// Overlap is a property of a pair of rectangles, not of the order they arrive in.
            #[test]
            fn intersection_is_commutative(left in any_rect(), right in any_rect()) {
                prop_assert_eq!(left.intersection(right), right.intersection(left));
            }

            /// `contains` and `intersection` are written independently — one compares a point
            /// against four edges, the other clamps four edges against four edges — and every
            /// hit test in the overlay depends on them agreeing.
            #[test]
            fn a_point_lies_in_the_intersection_exactly_when_it_lies_in_both(
                left in any_rect(),
                right in any_rect(),
                (x, y) in any_point(),
            ) {
                let in_both = left.contains(x, y) && right.contains(x, y);
                let in_intersection = left
                    .intersection(right)
                    .is_some_and(|intersection| intersection.contains(x, y));
                prop_assert_eq!(in_both, in_intersection);
            }

            /// An empty overlap has to be reported as `None` rather than as a zero-area rectangle,
            /// because `largest_intersection` ranks by area and a zero-area candidate would make a
            /// display that the selection does not touch eligible to win a tie.
            #[test]
            fn an_intersection_is_never_empty(left in any_rect(), right in any_rect()) {
                if let Some(intersection) = left.intersection(right) {
                    prop_assert!(intersection.width > 0 && intersection.height > 0);
                    prop_assert!(intersection.area() <= left.area().min(right.area()));
                }
            }

            /// The virtual desktop is exactly the bounding box of its displays: no display sticks
            /// out of it, and it is no larger than it has to be. Computed here by min/max against
            /// the implementation's successive-union fold.
            #[test]
            fn virtual_bounds_is_the_exact_bounding_box(bounds in any_display_bounds()) {
                let topology = topology_from(bounds.clone());
                let virtual_bounds = topology.virtual_bounds().expect("a non-empty topology");

                let left = bounds.iter().map(|rect| i64::from(rect.x)).min().expect("non-empty");
                let top = bounds.iter().map(|rect| i64::from(rect.y)).min().expect("non-empty");
                let right = bounds.iter().map(|rect| rect.right()).max().expect("non-empty");
                let bottom = bounds.iter().map(|rect| rect.bottom()).max().expect("non-empty");

                prop_assert_eq!(i64::from(virtual_bounds.x), left);
                prop_assert_eq!(i64::from(virtual_bounds.y), top);
                prop_assert_eq!(virtual_bounds.right(), right);
                prop_assert_eq!(virtual_bounds.bottom(), bottom);
                for rect in &bounds {
                    prop_assert!(virtual_bounds.intersection(*rect) == Some(*rect));
                }
            }

            /// Enumeration order is the display driver's business. Two daemons that enumerated the
            /// same displays in different orders must resolve a selection to the same display, or
            /// the same region captures different pixels on different runs — which is what the
            /// documented ID tie-break exists to prevent.
            #[test]
            fn display_resolution_does_not_depend_on_enumeration_order(
                bounds in any_display_bounds(),
                selection in any_rect(),
                (x, y) in any_point(),
            ) {
                // Reordering the displays, not the identities: a display carries its ID with it,
                // and the tie-break is on the ID rather than on the position. Reversing the bounds
                // under fixed IDs would be a different topology, not the same one enumerated
                // differently - which is how the first draft of this test failed.
                let displays = displays_from(bounds);
                let forwards = topology_of(displays.clone());
                let backwards = topology_of(displays.into_iter().rev().collect());

                prop_assert_eq!(
                    forwards.largest_intersection(selection).map(|display| display.bounds),
                    backwards.largest_intersection(selection).map(|display| display.bounds)
                );
                prop_assert_eq!(
                    forwards.containing_point(x, y).map(|display| display.bounds),
                    backwards.containing_point(x, y).map(|display| display.bounds)
                );
            }

            /// Whatever `largest_intersection` returns, no other display overlaps the selection by
            /// more; and it returns nothing only when nothing overlaps at all.
            #[test]
            fn largest_intersection_wins_on_area(
                bounds in any_display_bounds(),
                selection in any_rect(),
            ) {
                let topology = topology_from(bounds.clone());
                let best = topology.largest_intersection(selection);
                let areas = bounds
                    .iter()
                    .filter_map(|rect| rect.intersection(selection).map(|overlap| overlap.area()));

                match best {
                    None => prop_assert_eq!(areas.count(), 0),
                    Some(display) => {
                        let winning_area = display
                            .bounds
                            .intersection(selection)
                            .expect("the winner overlaps the selection")
                            .area();
                        prop_assert_eq!(areas.max(), Some(winning_area));
                    }
                }
            }

            /// Two displays placed edge to edge — the ordinary dual-monitor layout — divide the
            /// pixels between them exactly. A point in both is a pixel column captured twice; a
            /// point in neither is a column no display owns. `contains` is half-open for precisely
            /// this reason, and the convention is invisible to any test of one rectangle alone:
            /// checking `contains` against `intersection` cannot see it either, since a shifted
            /// convention shifts both of them together.
            #[test]
            fn edge_adjacent_displays_claim_every_pixel_exactly_once(
                left in any_rect(),
                neighbour_width in 1u32..=20_000,
                offset_from_seam in -2i64..=2,
                row in 0u32..=3,
            ) {
                let neighbour = Rect {
                    x: i32::try_from(left.right()).expect("generated bounds stay inside i32"),
                    y: left.y,
                    width: neighbour_width,
                    height: left.height,
                };
                // Sampled at the seam rather than uniformly. A random point in this coordinate
                // range lands on the shared edge about once in forty thousand tries, so a uniform
                // point tests the interiors of two rectangles and never the boundary between them
                // — which is the only place the half-open convention is observable.
                let x = i32::try_from(i64::from(neighbour.x) + offset_from_seam)
                    .expect("the seam and its neighbourhood stay inside i32");
                let y = left
                    .y
                    .saturating_add(i32::try_from(row.min(left.height - 1)).expect("a small row"));

                prop_assert!(
                    !(left.contains(x, y) && neighbour.contains(x, y)),
                    "({},{}) belongs to two displays at once", x, y
                );
                let inside_the_pair = i64::from(x) >= i64::from(left.x)
                    && i64::from(x) < neighbour.right()
                    && i64::from(y) >= i64::from(left.y)
                    && i64::from(y) < left.bottom();
                prop_assert_eq!(
                    inside_the_pair,
                    left.contains(x, y) || neighbour.contains(x, y),
                    "({},{}) belongs to neither display", x, y
                );
            }

            /// The case the ID tie-break exists for, which random layouts almost never produce:
            /// two candidates of exactly equal area. Two displays sharing one set of bounds tie
            /// against every selection by construction, so this is the only test here that
            /// actually reaches the tie-break, and it pins which way it goes rather than only
            /// that it is consistent.
            #[test]
            fn an_exact_tie_resolves_to_the_lowest_id_in_any_order(
                bounds in any_rect(),
                selection in any_rect(),
            ) {
                let displays = displays_from(vec![bounds, bounds]);
                let forwards = topology_of(displays.clone());
                let backwards = topology_of(displays.into_iter().rev().collect());

                let expected = bounds
                    .intersection(selection)
                    .map(|_| DisplayId("display-0".to_owned()));
                prop_assert_eq!(
                    forwards.largest_intersection(selection).map(|display| display.id.clone()),
                    expected.clone()
                );
                prop_assert_eq!(
                    backwards.largest_intersection(selection).map(|display| display.id.clone()),
                    expected
                );
            }

            /// Every display can be found by its own identifier, whatever the layout.
            #[test]
            fn every_display_resolves_by_its_own_id(bounds in any_display_bounds()) {
                let topology = topology_from(bounds);
                for display in topology.displays() {
                    prop_assert_eq!(
                        topology.resolve(&display.id).map(|found| &found.id),
                        Some(&display.id)
                    );
                }
            }
        }
    }
}
