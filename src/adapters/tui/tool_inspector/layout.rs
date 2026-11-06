//! One fitted allocation for both body-cache preparation and production paint.
use ratatui::layout::{Margin, Rect};

#[derive(Debug, Clone, Copy)]
pub struct Parts {
    pub identity: Option<Rect>,
    pub tabs: Rect,
    pub body: Rect,
    pub status: Option<Rect>,
    pub rail: Rect,
}

/// Reserve body >= 1, tabs, first action rail, status, then optional identity.
/// At the minimum supported full-composer sheet the three inner rows are the
/// tab strip, one body row and the Back rail; no optional detail displaces them.
pub fn parts(rect: Rect) -> Parts {
    let inner = rect.inner(Margin::new(1, 1));
    let h = inner.height;
    let tab_h = u16::from(h >= 2);
    let rail_h = u16::from(h >= 3);
    let status_h = u16::from(h >= 4);
    let identity_h = u16::from(h >= 5);
    let body_h = h.saturating_sub(tab_h + rail_h + status_h + identity_h);
    let row = |y, height| Rect::new(inner.x, y, inner.width, height);
    let mut y = inner.y;
    let identity = (identity_h != 0).then(|| row(y, identity_h));
    y += identity_h;
    let tabs = row(y, tab_h);
    y += tab_h;
    let body = row(y, body_h);
    y += body_h;
    let status = (status_h != 0).then(|| row(y, status_h));
    y += status_h;
    let rail = row(y, rail_h);
    Parts {
        identity,
        tabs,
        body,
        status,
        rail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn story21_5_fitted_parts_keep_primary_rows() {
        for h in 5..30 {
            let p = parts(Rect::new(3, 2, 60, h));
            assert!(p.body.height >= 1 && p.tabs.height == 1 && p.rail.height == 1);
            assert!(p.rail.bottom() < 2 + h);
            assert!(p.body.bottom() <= p.rail.y);
        }
        let p = parts(Rect::new(0, 0, 60, 5));
        assert!(p.identity.is_none() && p.status.is_none());
        assert_eq!(p.body.height, 1);
    }
}
