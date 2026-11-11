//! The shared list builder of the hub's list tabs (A2, A3): rows keyed by id, a selection that
//! is looked up by id after every refresh, a muted placeholder at the old position when the
//! selected row is gone (never rebound to a neighbour), and the master/detail split. Pure:
//! the widget paints exactly the rows these functions return, and the keys clamp against the
//! same numbers, so height equals paint.

use std::borrow::Cow;

/// One row of a list tab: a stable id, the one-line text and the detail preview text. The
/// texts are untrusted; the painter sanitizes them at the span builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McRow {
    pub id: String,
    pub text: String,
    pub detail: String,
}

impl McRow {
    #[must_use]
    pub fn new(id: &str, text: &str, detail: &str) -> Self {
        Self {
            id: id.to_owned(),
            text: text.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

/// The selection of one tab: the id, where it last sat and what it said (for the placeholder).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub id: Option<String>,
    pub at: usize,
    pub label: String,
}

/// How a selection resolves against the live rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// No rows and no selection.
    Empty,
    /// The selected row now sits at this index (the first row while nothing is selected).
    Found(usize),
    /// The selected id is gone; the placeholder takes this position of the painted list.
    Gone(usize),
}

/// Look the selected **id** up. A gone id is never rebound to the row now at its old index
/// (UX "removal shows unavailable"): the operator moves off the placeholder explicitly.
#[must_use]
pub fn resolve(rows: &[McRow], selection: &Selection) -> Resolved {
    match &selection.id {
        None if rows.is_empty() => Resolved::Empty,
        None => Resolved::Found(0),
        Some(id) => match rows.iter().position(|row| row.id == *id) {
            Some(index) => Resolved::Found(index),
            None => Resolved::Gone(selection.at.min(rows.len())),
        },
    }
}

/// Rows of the painted list: the rows, plus the placeholder while the selection is gone.
#[must_use]
pub fn view_len(rows: usize, resolved: Resolved) -> usize {
    rows + usize::from(matches!(resolved, Resolved::Gone(_)))
}

/// The position of the selection in the painted list.
#[must_use]
pub fn view_pos(resolved: Resolved) -> usize {
    match resolved {
        Resolved::Found(index) | Resolved::Gone(index) => index,
        Resolved::Empty => 0,
    }
}

/// One painted row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRow<'a> {
    pub text: Cow<'a, str>,
    pub selected: bool,
    pub gone: bool,
}

/// Row `index` of the painted list, placeholder included.
#[must_use]
pub fn view_row<'a>(
    rows: &'a [McRow],
    selection: &Selection,
    resolved: Resolved,
    index: usize,
) -> Option<ViewRow<'a>> {
    match resolved {
        Resolved::Empty => None,
        Resolved::Gone(at) if index == at => Some(ViewRow {
            text: Cow::Owned(format!("{} is no longer available", selection.label)),
            selected: true,
            gone: true,
        }),
        Resolved::Gone(at) => rows
            .get(if index > at { index - 1 } else { index })
            .map(|row| ViewRow {
                text: Cow::Borrowed(row.text.as_str()),
                selected: false,
                gone: false,
            }),
        Resolved::Found(selected) => rows.get(index).map(|row| ViewRow {
            text: Cow::Borrowed(row.text.as_str()),
            selected: index == selected,
            gone: false,
        }),
    }
}

/// The row a move to painted position `to` lands on; `None` when the selection stays (an empty
/// list, or the move ends on the placeholder itself).
#[must_use]
pub fn target(rows: usize, resolved: Resolved, to: usize) -> Option<usize> {
    match resolved {
        Resolved::Empty => None,
        Resolved::Found(_) => (rows > 0).then(|| to.min(rows - 1)),
        Resolved::Gone(at) => {
            let to = to.min(rows);
            match to.cmp(&at) {
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(to - 1),
                std::cmp::Ordering::Less => Some(to),
            }
        }
    }
}

/// Select row `index`, remembering where it sits and what it says.
pub fn select(selection: &mut Selection, rows: &[McRow], index: usize) {
    if let Some(row) = rows.get(index) {
        selection.id = Some(row.id.clone());
        selection.at = index;
        selection.label = row.text.clone();
    }
}

/// The first painted row that keeps `pos` visible in a window of `viewport` rows over `total`.
#[must_use]
pub fn keep_visible(top: usize, pos: usize, viewport: usize, total: usize) -> usize {
    let viewport = viewport.max(1);
    let mut top = top.min(total.saturating_sub(viewport));
    if pos < top {
        top = pos;
    } else if pos >= top + viewport {
        top = pos + 1 - viewport;
    }
    top
}

/// Inner width from which the list tabs show master and detail side by side.
pub const WIDE_INNER: u16 = 96;
const MASTER_MIN: u16 = 28;

/// `(master, detail)` widths for an inner width, one column between them for the divider;
/// `None` below [`WIDE_INNER`] (the list alone).
#[must_use]
pub fn split(inner_width: u16) -> Option<(u16, u16)> {
    (inner_width >= WIDE_INNER).then(|| {
        let master = ((u32::from(inner_width) * 40 / 100) as u16).max(MASTER_MIN);
        (master, inner_width - master - 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(ids: &[&str]) -> Vec<McRow> {
        ids.iter().map(|id| McRow::new(id, id, "")).collect()
    }

    #[test]
    fn story21_7_resolve_looks_the_id_up_and_never_rebinds() {
        let mut selection = Selection::default();
        assert_eq!(resolve(&[], &selection), Resolved::Empty);
        assert_eq!(resolve(&rows(&["a", "b"]), &selection), Resolved::Found(0));
        select(&mut selection, &rows(&["a", "b", "c"]), 1);
        assert_eq!(
            resolve(&rows(&["x", "b", "c"]), &selection),
            Resolved::Found(1)
        );
        assert_eq!(
            resolve(&rows(&["a", "c"]), &selection),
            Resolved::Gone(1),
            "the gone id keeps its old position and is not the row now sitting there"
        );
        assert_eq!(resolve(&[], &selection), Resolved::Gone(0));
    }

    #[test]
    fn story21_7_the_placeholder_takes_the_old_position_and_moves_land_around_it() {
        let selection = Selection {
            id: Some("b".to_owned()),
            at: 1,
            label: "beta".to_owned(),
        };
        let live = rows(&["a", "c", "d"]);
        let resolved = resolve(&live, &selection);
        assert_eq!(view_len(live.len(), resolved), 4);
        let painted: Vec<_> = (0..4)
            .filter_map(|index| view_row(&live, &selection, resolved, index))
            .map(|row| (row.text.into_owned(), row.gone))
            .collect();
        assert_eq!(
            painted,
            vec![
                ("a".to_owned(), false),
                ("beta is no longer available".to_owned(), true),
                ("c".to_owned(), false),
                ("d".to_owned(), false),
            ]
        );
        // Down lands on the row that took the old position, up on the one above; staying put
        // on the placeholder selects nothing.
        assert_eq!(target(live.len(), resolved, 2), Some(1));
        assert_eq!(target(live.len(), resolved, 0), Some(0));
        assert_eq!(target(live.len(), resolved, 1), None);
        assert_eq!(
            target(live.len(), resolved, 9),
            Some(2),
            "clamped to the last"
        );
    }

    #[test]
    fn story21_7_keep_visible_follows_the_selection_and_clamps_after_a_shrink() {
        assert_eq!(keep_visible(0, 5, 3, 10), 3);
        assert_eq!(keep_visible(4, 2, 3, 10), 2);
        assert_eq!(keep_visible(8, 9, 3, 10), 7, "never past the last window");
        assert_eq!(
            keep_visible(9, 0, 3, 2),
            0,
            "a list shorter than its window"
        );
    }

    #[test]
    fn story21_7_the_split_needs_96_inner_columns_and_keeps_the_master_at_28() {
        assert_eq!(split(95), None);
        assert_eq!(split(96), Some((38, 57)));
        assert_eq!(split(200), Some((80, 119)));
        for width in 96..=200u16 {
            let (master, detail) = split(width).expect("wide");
            assert!(master >= 28);
            assert_eq!(master + 1 + detail, width, "{width}");
        }
    }
}
