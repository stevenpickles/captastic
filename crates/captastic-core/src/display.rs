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
}
