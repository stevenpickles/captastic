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
//! and never one pixel less.

use captastic_core::Rect;

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

#[cfg(test)]
mod tests {
    use super::*;

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
                    rect: Rect {
                        x: index as i32,
                        y: 0,
                        width: 10,
                        height: 10,
                    },
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
}
