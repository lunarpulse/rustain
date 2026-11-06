//! Shared vertical inspector navigation. The viewer never owns Stage state.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewer {
    pub top: usize,
    pub follow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerCmd {
    LineDown,
    LineUp,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Top,
    Tail,
}

/// Apply a registry command against the last fitted body dimensions.
pub fn apply(viewer: &mut Viewer, command: ViewerCmd, rows: usize, viewport: usize) {
    use ViewerCmd::*;
    let maximum = rows.saturating_sub(viewport);
    let top = viewer.top.min(maximum);
    viewer.top = match command {
        LineDown => top.saturating_add(1),
        LineUp => top.saturating_sub(1),
        HalfDown => top.saturating_add((viewport / 2).max(1)),
        HalfUp => top.saturating_sub((viewport / 2).max(1)),
        PageDown => top.saturating_add(viewport.saturating_sub(1).max(1)),
        PageUp => top.saturating_sub(viewport.saturating_sub(1).max(1)),
        Top => 0,
        Tail => maximum,
    }
    .min(maximum);
    viewer.follow = command == Tail;
}

/// Painting clamps again: keys can precede the first fitted paint or a resize.
pub fn slice(viewer: &Viewer, rows: usize, viewport: usize) -> std::ops::Range<usize> {
    let top = viewer.top.min(rows.saturating_sub(viewport));
    top..top.saturating_add(viewport).min(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_5_viewer_movements_and_follow_by_value() {
        use ViewerCmd::*;
        for (command, expected) in [
            (LineDown, 21),
            (LineUp, 19),
            (HalfDown, 26),
            (HalfUp, 14),
            (PageDown, 31),
            (PageUp, 9),
            (Top, 0),
            (Tail, 88),
        ] {
            let mut viewer = Viewer {
                top: 20,
                follow: true,
            };
            apply(&mut viewer, command, 100, 12);
            assert_eq!(viewer.top, expected, "{command:?}");
            assert_eq!(viewer.follow, command == Tail, "{command:?}");
        }
    }

    #[test]
    fn story21_5_viewer_clamps_short_empty_zero_and_overflow_edges() {
        use ViewerCmd::*;
        for command in [
            LineDown, LineUp, HalfDown, HalfUp, PageDown, PageUp, Top, Tail,
        ] {
            for (rows, viewport) in [(0, 12), (5, 12), (12, 12)] {
                let mut viewer = Viewer {
                    top: usize::MAX,
                    follow: false,
                };
                apply(&mut viewer, command, rows, viewport);
                assert_eq!(viewer.top, 0, "{command:?} {rows}/{viewport}");
                assert!(slice(&viewer, rows, viewport).end <= rows);
            }
            let mut viewer = Viewer {
                top: usize::MAX,
                follow: false,
            };
            apply(&mut viewer, command, usize::MAX, 1);
            assert!(viewer.top < usize::MAX);
        }
        let mut v = Viewer::default();
        apply(&mut v, HalfDown, 10, 0);
        assert_eq!(v.top, 1);
        apply(&mut v, PageDown, 10, 1);
        assert_eq!(v.top, 2);
    }

    #[test]
    fn story21_5_viewer_repeated_motion_stops_and_paint_clamps_again() {
        let mut v = Viewer::default();
        for _ in 0..200 {
            apply(&mut v, ViewerCmd::LineDown, 100, 12);
        }
        assert_eq!(v.top, 88);
        assert_eq!(slice(&v, 100, 12), 88..100);
        assert_eq!(slice(&v, 5, 12), 0..5);
        for _ in 0..200 {
            apply(&mut v, ViewerCmd::LineUp, 100, 12);
        }
        assert_eq!(v.top, 0);
    }
}
