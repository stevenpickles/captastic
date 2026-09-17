//! Edge snapping for the region tool: the inventory of edges a region can be pulled onto, and
//! the arithmetic that pulls it.
//!
//! Nothing here touches a window, a device context, a clock, or Win32 at all. The shell builds the
//! [`SnapTargets`] inventory by enumerating windows and hands it to the machine as an input; this
//! module is the pure half, so every rule — which edge wins a tie, what happens at the threshold,
//! and above all that a snapped right edge is *exclusive* — is decided in tests rather than on a
//! desktop.
//!
//! Every coordinate here is half-open: a rectangle occupies `[left, right)`, so snapping a
//! region's right edge to a window's right edge means `region.x + region.width == window.right()`
//! and never one pixel less. The guide *line* for such an edge is a different thing — the last
//! pixel inside the region is at `right - 1` — which is what [`SnapGuide::trailing`] records.

use captastic_core::Rect;

use super::machine::ResizeHandle;

/// How close an edge has to be, in device-independent pixels, before it is pulled onto a target.
///
/// Eight DIPs is a little under the 9-DIP resize-handle hit radius: close enough that aiming at a
/// window border lands on it, small enough that a deliberate placement one handle-width away is
/// never overridden. Scaled through `UiMetrics::px`, so it is the same physical distance on every
/// display.
pub(super) const SNAP_THRESHOLD_DIP: i32 = 8;

/// What a snap target is. Used to order the inventory, and to say in the log what a run had to
/// work with when a snap did or did not happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnapTargetKind {
    /// A visible application window's frame, as the user sees it.
    Window,
    /// The display's work area: the desktop minus the taskbar and any registered appbars.
    WorkArea,
    /// The captured display itself.
    Display,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SnapTarget {
    pub(super) rect: Rect,
    pub(super) kind: SnapTargetKind,
}

/// The ordered inventory a region is snapped against.
///
/// Order is meaning, not decoration: windows come first in front-to-back z-order, then the work
/// area, then the display. Scanning prefers a candidate only on a *strictly* smaller distance, so
/// two edges the same distance away resolve to whichever the user is looking at — the topmost
/// window — and the display, which every region is inside of, can never win a tie against a real
/// window edge.
#[derive(Clone, Debug, Default)]
pub(super) struct SnapTargets {
    targets: Vec<SnapTarget>,
}

impl SnapTargets {
    pub(super) fn new(targets: Vec<SnapTarget>) -> Self {
        Self { targets }
    }

    pub(super) fn len(&self) -> usize {
        self.targets.len()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &SnapTarget> {
        self.targets.iter()
    }
}

/// Which way a guide line runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnapAxis {
    /// A vertical line at a fixed x, produced by snapping a left or right edge.
    X,
    /// A horizontal line at a fixed y, produced by snapping a top or bottom edge.
    Y,
}

impl SnapAxis {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Y => 1,
        }
    }
}

/// One guide line to paint: the shared coordinate, and how far along the target edge it runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SnapGuide {
    pub(super) axis: SnapAxis,
    /// The shared half-open coordinate. For a trailing guide this is one *past* the last pixel of
    /// the selection, so the line itself belongs at `position - 1`.
    pub(super) position: i32,
    /// The winning target edge's extent along the other axis, half-open.
    pub(super) span: (i32, i32),
    /// True when the region edge that snapped was its exclusive right or bottom edge.
    pub(super) trailing: bool,
}

/// At most one guide per axis: `[x, y]`.
pub(super) type ActiveSnaps = [Option<SnapGuide>; 2];

/// No guide on either axis — what a drag with Ctrl held, or with snapping off, always reports.
pub(super) const NO_SNAPS: ActiveSnaps = [None, None];

/// A rectangle's four half-open edges, in desktop coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RectEdges {
    pub(super) left: i64,
    pub(super) top: i64,
    pub(super) right: i64,
    pub(super) bottom: i64,
}

impl RectEdges {
    pub(super) fn of(rect: Rect) -> Self {
        Self {
            left: i64::from(rect.x),
            top: i64::from(rect.y),
            right: rect.right(),
            bottom: rect.bottom(),
        }
    }
}

/// Which edges of a rectangle an operation is allowed to move.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct EdgeMask {
    pub(super) left: bool,
    pub(super) top: bool,
    pub(super) right: bool,
    pub(super) bottom: bool,
}

impl EdgeMask {
    /// Every edge: a region being moved as a whole offers all four sides to the inventory.
    pub(super) const ALL: Self = Self {
        left: true,
        top: true,
        right: true,
        bottom: true,
    };

    /// The edges a resize handle moves — and only those. Dragging the east handle must not pull
    /// the region's left edge onto a window, because the user is not touching that edge.
    pub(super) const fn for_handle(handle: ResizeHandle) -> Self {
        match handle {
            ResizeHandle::NorthWest => Self {
                left: true,
                top: true,
                right: false,
                bottom: false,
            },
            ResizeHandle::North => Self {
                left: false,
                top: true,
                right: false,
                bottom: false,
            },
            ResizeHandle::NorthEast => Self {
                left: false,
                top: true,
                right: true,
                bottom: false,
            },
            ResizeHandle::East => Self {
                left: false,
                top: false,
                right: true,
                bottom: false,
            },
            ResizeHandle::SouthEast => Self {
                left: false,
                top: false,
                right: true,
                bottom: true,
            },
            ResizeHandle::South => Self {
                left: false,
                top: false,
                right: false,
                bottom: true,
            },
            ResizeHandle::SouthWest => Self {
                left: true,
                top: false,
                right: false,
                bottom: true,
            },
            ResizeHandle::West => Self {
                left: true,
                top: false,
                right: false,
                bottom: false,
            },
        }
    }

    /// Whether this mask moves anything on the given axis.
    pub(super) const fn covers(self, axis: SnapAxis) -> bool {
        match axis {
            SnapAxis::X => self.left || self.right,
            SnapAxis::Y => self.top || self.bottom,
        }
    }
}

/// A target edge that won a scan: where it sits, and how far it runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SnapHit {
    pub(super) position: i64,
    pub(super) span: (i64, i64),
}

/// The nearest target edge on `axis` within `threshold` of `value`, or `None`.
///
/// A candidate displaces the incumbent only on a *strictly* smaller distance, so a distance tie
/// keeps the earlier target and the inventory's front-to-back order decides it.
pub(super) fn snap_coordinate(
    value: i64,
    targets: &SnapTargets,
    axis: SnapAxis,
    threshold: i64,
) -> Option<SnapHit> {
    let mut best: Option<(i64, SnapHit)> = None;
    for target in targets.iter() {
        for (position, span) in target_edges(target.rect, axis) {
            let distance = (position - value).abs();
            if distance > threshold {
                continue;
            }
            if best.is_none_or(|(closest, _)| distance < closest) {
                best = Some((distance, SnapHit { position, span }));
            }
        }
    }
    best.map(|(_, hit)| hit)
}

/// Both of a target's edges on one axis, each with the extent of that edge along the other axis.
fn target_edges(rect: Rect, axis: SnapAxis) -> [(i64, (i64, i64)); 2] {
    match axis {
        SnapAxis::X => {
            let span = (i64::from(rect.y), rect.bottom());
            [(i64::from(rect.x), span), (rect.right(), span)]
        }
        SnapAxis::Y => {
            let span = (i64::from(rect.x), rect.right());
            [(i64::from(rect.y), span), (rect.bottom(), span)]
        }
    }
}

/// Snaps a bare point — the free corner of a rubber-band drag — on both axes.
pub(super) fn snap_point(
    x: i64,
    y: i64,
    targets: &SnapTargets,
    threshold: i64,
) -> ((i64, i64), [Option<SnapHit>; 2]) {
    let horizontal = snap_coordinate(x, targets, SnapAxis::X, threshold);
    let vertical = snap_coordinate(y, targets, SnapAxis::Y, threshold);
    (
        (
            horizontal.map_or(x, |hit| hit.position),
            vertical.map_or(y, |hit| hit.position),
        ),
        [horizontal, vertical],
    )
}

/// Moves the masked edges of `edges` onto the nearest target edges.
///
/// When both edges on an axis are in the mask the closer one wins, and a tie keeps the leading
/// edge — so a region exactly as wide as a window does not flicker between its two borders.
/// Returns the adjusted edges and the winning hit per axis; whether a hit survives into a guide is
/// the caller's business, because a clamp applied afterwards can invalidate it.
pub(super) fn snap_edges(
    edges: RectEdges,
    mask: EdgeMask,
    targets: &SnapTargets,
    threshold: i64,
) -> (RectEdges, [Option<SnapHit>; 2]) {
    let mut snapped = edges;
    let mut hits = [None, None];
    for (axis, leading, trailing, leading_allowed, trailing_allowed) in [
        (SnapAxis::X, edges.left, edges.right, mask.left, mask.right),
        (SnapAxis::Y, edges.top, edges.bottom, mask.top, mask.bottom),
    ] {
        let leading_hit = leading_allowed
            .then(|| snap_coordinate(leading, targets, axis, threshold))
            .flatten()
            .map(|hit| ((hit.position - leading).abs(), false, hit));
        let trailing_hit = trailing_allowed
            .then(|| snap_coordinate(trailing, targets, axis, threshold))
            .flatten()
            .map(|hit| ((hit.position - trailing).abs(), true, hit));
        let winner = match (leading_hit, trailing_hit) {
            (Some(leading_hit), Some(trailing_hit)) => Some(if trailing_hit.0 < leading_hit.0 {
                trailing_hit
            } else {
                leading_hit
            }),
            (Some(hit), None) | (None, Some(hit)) => Some(hit),
            (None, None) => None,
        };
        if let Some((_, is_trailing, hit)) = winner {
            match (axis, is_trailing) {
                (SnapAxis::X, false) => snapped.left = hit.position,
                (SnapAxis::X, true) => snapped.right = hit.position,
                (SnapAxis::Y, false) => snapped.top = hit.position,
                (SnapAxis::Y, true) => snapped.bottom = hit.position,
            }
            hits[axis.index()] = Some(hit);
        }
    }
    (snapped, hits)
}

/// How far a whole rectangle should move on each axis so one of its sides lands on a target edge:
/// the arithmetic behind dragging a region about without resizing it.
pub(super) fn snap_translation(
    edges: RectEdges,
    targets: &SnapTargets,
    threshold: i64,
) -> [Option<(i64, SnapHit)>; 2] {
    let (snapped, hits) = snap_edges(edges, EdgeMask::ALL, targets, threshold);
    [
        hits[SnapAxis::X.index()].map(|hit| {
            (
                translation_on(edges.left, edges.right, snapped, SnapAxis::X),
                hit,
            )
        }),
        hits[SnapAxis::Y.index()].map(|hit| {
            (
                translation_on(edges.top, edges.bottom, snapped, SnapAxis::Y),
                hit,
            )
        }),
    ]
}

fn translation_on(leading: i64, trailing: i64, snapped: RectEdges, axis: SnapAxis) -> i64 {
    let (snapped_leading, snapped_trailing) = match axis {
        SnapAxis::X => (snapped.left, snapped.right),
        SnapAxis::Y => (snapped.top, snapped.bottom),
    };
    if snapped_leading == leading {
        snapped_trailing - trailing
    } else {
        snapped_leading - leading
    }
}

/// Turns the hits a snap produced into the guides a *final* rectangle actually justifies.
///
/// A hit only becomes a guide when the finished rectangle still has an edge sitting on it.
/// Clamping to the display and enforcing the minimum region size both happen after snapping and
/// can undo it, and a guide left over from an edge that moved back would draw a line at a
/// coordinate the selection does not touch.
///
/// The test is against *either* edge on the axis rather than against the one that was snapped.
/// That is deliberate: a line is drawn when the selection really does sit on that target
/// coordinate, and which of its two edges happens to be there is not something the line claims.
pub(super) fn guides_for(rect: Rect, hits: [Option<SnapHit>; 2]) -> ActiveSnaps {
    let edges = RectEdges::of(rect);
    [
        guide_for(
            hits[SnapAxis::X.index()],
            SnapAxis::X,
            edges.left,
            edges.right,
        ),
        guide_for(
            hits[SnapAxis::Y.index()],
            SnapAxis::Y,
            edges.top,
            edges.bottom,
        ),
    ]
}

fn guide_for(
    hit: Option<SnapHit>,
    axis: SnapAxis,
    leading: i64,
    trailing: i64,
) -> Option<SnapGuide> {
    let hit = hit?;
    // Leading is tested first, so a rectangle at the minimum size whose edges are close together
    // still draws its guide on the edge the user is looking at rather than one past it.
    let is_trailing = if hit.position == leading {
        false
    } else if hit.position == trailing {
        true
    } else {
        return None;
    };
    Some(SnapGuide {
        axis,
        position: i32::try_from(hit.position).ok()?,
        span: (
            i32::try_from(hit.span.0).ok()?,
            i32::try_from(hit.span.1).ok()?,
        ),
        trailing: is_trailing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn window(x: i32, y: i32, width: u32, height: u32) -> SnapTarget {
        SnapTarget {
            rect: rect(x, y, width, height),
            kind: SnapTargetKind::Window,
        }
    }

    fn display(x: i32, y: i32, width: u32, height: u32) -> SnapTarget {
        SnapTarget {
            rect: rect(x, y, width, height),
            kind: SnapTargetKind::Display,
        }
    }

    #[test]
    fn an_inventory_keeps_the_order_it_was_built_in() {
        // Front-to-back z-order is the tie-break, so the inventory must not reorder anything.
        let kinds = [
            SnapTargetKind::Window,
            SnapTargetKind::Window,
            SnapTargetKind::WorkArea,
            SnapTargetKind::Display,
        ];
        let targets = SnapTargets::new(
            kinds
                .iter()
                .enumerate()
                .map(|(index, kind)| SnapTarget {
                    rect: rect(index as i32, 0, 10, 10),
                    kind: *kind,
                })
                .collect(),
        );
        assert_eq!(targets.len(), kinds.len());
        assert_eq!(
            targets.iter().map(|target| target.kind).collect::<Vec<_>>(),
            kinds
        );
        assert_eq!(
            targets
                .iter()
                .map(|target| target.rect.x)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn an_empty_inventory_is_the_default() {
        assert_eq!(SnapTargets::default().len(), 0);
        assert!(SnapTargets::default().iter().next().is_none());
    }

    #[test]
    fn a_right_edge_snaps_onto_the_exclusive_window_edge() {
        // The window occupies columns 100..=299; its right edge is the exclusive 300.
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        let edges = RectEdges {
            left: 50,
            top: 50,
            right: 296,
            bottom: 150,
        };
        let (snapped, hits) = snap_edges(edges, EdgeMask::ALL, &targets, 8);
        assert_eq!(snapped.right, 300, "never 299: right() is exclusive");
        let region = Rect::from_edges(snapped.left, snapped.top, snapped.right, snapped.bottom)
            .expect("a positive span");
        assert_eq!(
            i64::from(region.x) + i64::from(region.width),
            300,
            "x + width is the window's right edge exactly"
        );
        let guide = guides_for(region, hits)[SnapAxis::X.index()].expect("a vertical guide");
        assert_eq!(guide.position, 300);
        assert!(guide.trailing, "the exclusive edge paints at position - 1");
        assert_eq!(guide.span, (100, 300));
    }

    #[test]
    fn a_left_edge_snaps_without_a_trailing_offset() {
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        let edges = RectEdges {
            left: 103,
            top: 400,
            right: 500,
            bottom: 600,
        };
        let (snapped, hits) = snap_edges(edges, EdgeMask::ALL, &targets, 8);
        assert_eq!(snapped.left, 100);
        let region = Rect::from_edges(snapped.left, snapped.top, snapped.right, snapped.bottom)
            .expect("a positive span");
        let guide = guides_for(region, hits)[SnapAxis::X.index()].expect("a vertical guide");
        assert_eq!(guide.position, 100);
        assert!(!guide.trailing);
    }

    #[test]
    fn nothing_moves_beyond_the_threshold_and_snapping_is_idempotent() {
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        let far = RectEdges {
            left: 50,
            top: 50,
            right: 289,
            bottom: 150,
        };
        let (unmoved, hits) = snap_edges(far, EdgeMask::ALL, &targets, 8);
        assert_eq!(unmoved, far, "11 px away is outside an 8 px threshold");
        assert_eq!(hits, [None, None]);

        let near = RectEdges {
            left: 50,
            top: 50,
            right: 296,
            bottom: 150,
        };
        let (once, _) = snap_edges(near, EdgeMask::ALL, &targets, 8);
        let (twice, _) = snap_edges(once, EdgeMask::ALL, &targets, 8);
        assert_eq!(once, twice, "an already-snapped edge does not drift");
    }

    #[test]
    fn the_topmost_window_wins_a_tie_and_the_display_never_does() {
        // Two window edges equidistant from 100, front to back, plus a display edge as close.
        let targets = SnapTargets::new(vec![
            window(104, 0, 100, 100),
            window(96, 0, 100, 100),
            display(96, 0, 1920, 1080),
        ]);
        let hit = snap_coordinate(100, &targets, SnapAxis::X, 8).expect("a hit");
        assert_eq!(hit.position, 104, "the first, topmost target wins the tie");
        assert_eq!(
            hit.span,
            (0, 100),
            "and it is that window's edge that spans"
        );
    }

    #[test]
    fn only_the_handles_own_edges_move() {
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        let edges = RectEdges {
            left: 103,
            top: 103,
            right: 296,
            bottom: 296,
        };
        let (east, _) = snap_edges(edges, EdgeMask::for_handle(ResizeHandle::East), &targets, 8);
        assert_eq!(east.right, 300, "the east handle moves the right edge");
        assert_eq!(
            (east.left, east.top, east.bottom),
            (103, 103, 296),
            "and leaves every other edge alone"
        );

        let (corner, _) = snap_edges(
            edges,
            EdgeMask::for_handle(ResizeHandle::NorthWest),
            &targets,
            8,
        );
        assert_eq!((corner.left, corner.top), (100, 100));
        assert_eq!((corner.right, corner.bottom), (296, 296));

        assert!(EdgeMask::for_handle(ResizeHandle::East).covers(SnapAxis::X));
        assert!(!EdgeMask::for_handle(ResizeHandle::East).covers(SnapAxis::Y));
        assert!(EdgeMask::for_handle(ResizeHandle::South).covers(SnapAxis::Y));
        assert!(!EdgeMask::for_handle(ResizeHandle::South).covers(SnapAxis::X));
    }

    #[test]
    fn a_translation_moves_the_nearer_side_and_leaves_the_far_axis_alone() {
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        // 40 wide: the right edge is 4 px from the window's right edge, the left edge is far from
        // every target, so only the right side is in range and the whole rectangle follows it.
        let edges = RectEdges {
            left: 256,
            top: 500,
            right: 296,
            bottom: 540,
        };
        let [horizontal, vertical] = snap_translation(edges, &targets, 8);
        let (delta, hit) = horizontal.expect("a horizontal translation");
        assert_eq!(delta, 4);
        assert_eq!(hit.position, 300);
        assert!(vertical.is_none(), "nothing is within reach vertically");
    }

    #[test]
    fn a_guide_a_later_clamp_invalidated_is_dropped() {
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        let (_, hits) = snap_edges(
            RectEdges {
                left: 50,
                top: 50,
                right: 296,
                bottom: 150,
            },
            EdgeMask::ALL,
            &targets,
            8,
        );
        assert!(hits[SnapAxis::X.index()].is_some());
        // The rectangle that actually survived was clamped back to 280, so nothing sits on 300.
        let clamped = rect(50, 50, 230, 100);
        assert_eq!(clamped.right(), 280);
        assert_eq!(guides_for(clamped, hits)[SnapAxis::X.index()], None);
    }

    #[test]
    fn a_bare_point_snaps_on_both_axes_and_reports_nothing_out_of_reach() {
        let targets = SnapTargets::new(vec![window(100, 100, 200, 200)]);
        assert_eq!(snap_point(104, 297, &targets, 8).0, (100, 300));
        let (unmoved, hits) = snap_point(500, 700, &targets, 8);
        assert_eq!(unmoved, (500, 700), "out of reach on both axes");
        assert_eq!(hits, [None, None]);
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn any_rect() -> impl Strategy<Value = Rect> {
            (
                -4_000i32..=4_000,
                -4_000i32..=4_000,
                1u32..=4_000,
                1u32..=4_000,
            )
                .prop_map(|(x, y, width, height)| Rect {
                    x,
                    y,
                    width,
                    height,
                })
        }

        fn any_targets() -> impl Strategy<Value = SnapTargets> {
            proptest::collection::vec(any_rect(), 0..=6).prop_map(|rects| {
                SnapTargets::new(
                    rects
                        .into_iter()
                        .map(|rect| SnapTarget {
                            rect,
                            kind: SnapTargetKind::Window,
                        })
                        .collect(),
                )
            })
        }

        proptest! {
            /// A snap is a small correction, never a jump: no edge travels further than the
            /// threshold, and every edge it moves lands exactly on some target edge. The second
            /// half is what makes "snapped" mean anything at all.
            #[test]
            fn snapping_moves_each_edge_at_most_the_threshold_and_onto_a_target(
                rect in any_rect(),
                targets in any_targets(),
                threshold in 0i64..=32,
            ) {
                let edges = RectEdges::of(rect);
                let (snapped, _) = snap_edges(edges, EdgeMask::ALL, &targets, threshold);
                let moves = [
                    (snapped.left, edges.left, SnapAxis::X),
                    (snapped.right, edges.right, SnapAxis::X),
                    (snapped.top, edges.top, SnapAxis::Y),
                    (snapped.bottom, edges.bottom, SnapAxis::Y),
                ];
                for (after, before, axis) in moves {
                    prop_assert!((after - before).abs() <= threshold);
                    if after != before {
                        prop_assert!(
                            targets.iter().any(|target| {
                                target_edges(target.rect, axis)
                                    .iter()
                                    .any(|(position, _)| *position == after)
                            }),
                            "an edge moved to {}, which is not a target edge", after
                        );
                    }
                }
            }

            /// Snapping an already-snapped rectangle is a no-op. Pointer moves arrive by the
            /// hundred during one drag, and a rule that crept by a pixel each time would walk a
            /// stationary region across the screen.
            #[test]
            fn snapping_is_idempotent(
                rect in any_rect(),
                targets in any_targets(),
                threshold in 0i64..=32,
            ) {
                let once = snap_edges(RectEdges::of(rect), EdgeMask::ALL, &targets, threshold).0;
                let twice = snap_edges(once, EdgeMask::ALL, &targets, threshold).0;
                prop_assert_eq!(once, twice);
            }

            /// An empty inventory — a bare desktop, and what a failed enumeration produces —
            /// leaves every edge exactly where it was.
            #[test]
            fn no_targets_means_no_movement(rect in any_rect(), threshold in 0i64..=32) {
                let edges = RectEdges::of(rect);
                let targets = SnapTargets::default();
                prop_assert_eq!(snap_edges(edges, EdgeMask::ALL, &targets, threshold).0, edges);
                prop_assert_eq!(
                    snap_point(edges.left, edges.top, &targets, threshold).0,
                    (edges.left, edges.top)
                );
                prop_assert_eq!(snap_translation(edges, &targets, threshold), [None, None]);
            }

            /// A guide is only ever emitted for a coordinate the finished rectangle really has an
            /// edge on, and `trailing` says which of the two it is.
            #[test]
            fn every_guide_sits_on_an_edge_of_the_rectangle_it_describes(
                rect in any_rect(),
                targets in any_targets(),
                threshold in 0i64..=32,
            ) {
                let (_, hits) = snap_edges(RectEdges::of(rect), EdgeMask::ALL, &targets, threshold);
                let edges = RectEdges::of(rect);
                for guide in guides_for(rect, hits).into_iter().flatten() {
                    let position = i64::from(guide.position);
                    let (leading, trailing) = match guide.axis {
                        SnapAxis::X => (edges.left, edges.right),
                        SnapAxis::Y => (edges.top, edges.bottom),
                    };
                    prop_assert_eq!(position, if guide.trailing { trailing } else { leading });
                    prop_assert!(guide.span.0 < guide.span.1);
                }
            }

            /// Translating by what `snap_translation` asks for really does put a side on the
            /// target edge, and never changes the rectangle's size.
            #[test]
            fn a_translation_lands_a_side_on_its_target(
                rect in any_rect(),
                targets in any_targets(),
                threshold in 0i64..=32,
            ) {
                let edges = RectEdges::of(rect);
                let deltas = snap_translation(edges, &targets, threshold);
                let dx = deltas[SnapAxis::X.index()].map_or(0, |(delta, _)| delta);
                let dy = deltas[SnapAxis::Y.index()].map_or(0, |(delta, _)| delta);
                prop_assert!(dx.abs() <= threshold && dy.abs() <= threshold);
                let moved = rect.translated(dx, dy);
                prop_assert_eq!(moved.width, rect.width);
                prop_assert_eq!(moved.height, rect.height);
                if let Some((_, hit)) = deltas[SnapAxis::X.index()] {
                    prop_assert!(
                        i64::from(moved.x) == hit.position || moved.right() == hit.position
                    );
                }
                if let Some((_, hit)) = deltas[SnapAxis::Y.index()] {
                    prop_assert!(
                        i64::from(moved.y) == hit.position || moved.bottom() == hit.position
                    );
                }
            }
        }
    }
}
