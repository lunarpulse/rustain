//! Link presentation and the host-suffix rule (D11, AC13, owner ruling
//! OR-21-12-1).
//!
//! Pure functions only: nothing here touches a buffer or a terminal. The label
//! and the destination of a model-written link are hostile input. The only
//! URL that ever reaches an OSC 8 envelope is `login_ui::safe_url`'s
//! re-serialisation (http/https, no control character or whitespace), and the
//! only text derived from a destination is the host that serialisation
//! carries, never the userinfo.

use std::sync::Arc;

use unicode_width::UnicodeWidthChar;

use crate::adapters::tui::login_ui::safe_url;

/// How links are presented (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPresentation {
    /// `label (url)`, the URL once when the label already is the URL. Task
    /// detail and every surface where the full destination must stay visible.
    Explicit,
    /// The Stage: the label, then a dim ` (host)` suffix unless the host is
    /// already named by the label. A destination `safe_url` rejects renders as
    /// `Explicit`. With `osc8` the label cells are also returned as
    /// [`LinkRun`]s; a caller sets it only if it encodes every returned run.
    Labeled { osc8: bool },
}

/// One OSC 8 run: `width` cells starting at `start_cell` of line `line`
/// (relative to the rendered lines), covering label cells of one physical
/// line segment only, never a host suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRun {
    pub line: usize,
    pub start_cell: usize,
    pub width: usize,
    /// `safe_url` output: safe to put in an OSC 8 envelope as is.
    pub url: Arc<str>,
}

/// The longest linked target in bytes: every label glyph carries the whole URL
/// in its OSC 8 envelope on every frame. A longer one stays visible, unlinked.
const MAX_TARGET_BYTES: usize = 2048;

/// The target that may be linked: `dest` re-serialised by `safe_url`.
#[must_use]
pub fn safe_target(dest: &str) -> Option<String> {
    safe_url(dest).filter(|safe| safe.len() <= MAX_TARGET_BYTES)
}

/// The dim suffix text (` (host)`) for a link with the safe target `safe` and
/// the raw destination `dest`, or `None` when the label already shows the
/// destination or names its host (AC13).
///
/// * The host is `url::Url::host_str` of the `safe_url` serialisation:
///   userinfo is never the host, an internationalised host is its ASCII
///   `xn--` form, IPv6 literals keep their brackets, ports are not shown.
/// * Trailing dots and one leading `www.` are dropped, in that order, from
///   the displayed host and from both sides of the comparison (`www.` alone
///   is therefore the host `www`, never empty).
/// * The label names the host when one of its maximal `[a-z0-9.-]` tokens
///   (of the visible label, lowercased) equals the host. A substring is not
///   enough.
/// * A label equal to the destination (autolinks) never gets a suffix.
#[must_use]
pub fn host_suffix(safe: &str, dest: &str, label: &str) -> Option<String> {
    if label == dest || label == safe {
        return None;
    }
    let url = url::Url::parse(safe).ok()?;
    let host = bare(url.host_str()?);
    if host.is_empty() || label_names_host(label, host) {
        return None;
    }
    Some(format!(" ({host})"))
}

fn bare(host: &str) -> &str {
    let host = host.trim_end_matches('.');
    host.strip_prefix("www.").unwrap_or(host)
}

fn label_names_host(label: &str, host: &str) -> bool {
    // Tokens are the VISIBLE label, as `clean_prose` paints it: a tab is a
    // space; control, zero-width and soft-hyphen characters are not painted.
    // ASCII lowercasing only: a Unicode label never folds into an ASCII host
    // (homograph exposure), and non-ASCII characters split tokens.
    let visible: String = label
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\u{AD}' => None,
            _ => c.width().is_some_and(|w| w > 0).then_some(c),
        })
        .collect();
    visible
        .to_ascii_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-'))
        .map(bare)
        .any(|token| !token.is_empty() && token == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The painted suffix, `""` when none is shown.
    fn shown(dest: &str, label: &str) -> String {
        safe_target(dest)
            .and_then(|safe| host_suffix(&safe, dest, label))
            .unwrap_or_default()
    }

    #[test]
    fn userinfo_is_never_the_host() {
        assert_eq!(
            shown("https://claude.ai@evil.test/", "claude.ai"),
            " (evil.test)"
        );
    }

    #[test]
    fn idn_hosts_show_their_ascii_form_and_a_unicode_label_never_suppresses_it() {
        assert_eq!(shown("https://例え.test/", "例え"), " (xn--r8jz45g.test)");
        // The ASCII form named in the label is the host.
        assert_eq!(shown("https://例え.test/", "xn--r8jz45g.test"), "");
    }

    #[test]
    fn one_leading_www_is_dropped_on_both_sides() {
        assert_eq!(shown("https://www.example.com/x", "example.com"), "");
        assert_eq!(shown("https://www.example.com", "docs"), " (example.com)");
        assert_eq!(shown("https://example.com/", "www.example.com"), "");
        // Only one www. is dropped.
        let twice = shown("https://www.www.example.com/", "example.com");
        assert_eq!(twice, " (www.example.com)");
    }

    #[test]
    fn the_whole_domain_token_must_match() {
        assert_eq!(shown("https://evil.test", "notevil.test"), " (evil.test)");
        assert_eq!(shown("https://evil.test", "evil.test.com"), " (evil.test)");
        assert_eq!(
            shown("https://example.com/b", "see https://example.com/a"),
            ""
        );
        assert_eq!(shown("https://evil.test", "EVIL.test."), "");
        assert_eq!(shown("https://evil.test", "evil_test"), " (evil.test)");
    }

    #[test]
    fn autolinks_and_url_labels_get_no_suffix() {
        for url in [
            "https://example.com/a",
            "https://claude.ai@evil.test/",
            "https://e.test",
        ] {
            // The label already shows the whole destination.
            assert_eq!(shown(url, url), "", "{url}");
        }
    }

    #[test]
    fn ip_literals_display_as_serialised_and_ports_are_not_shown() {
        let ip = "http://127.0.0.1:8080/p";
        assert_eq!(shown(ip, "ip"), " (127.0.0.1)");
        assert_eq!(shown(ip, "127.0.0.1"), "");
        assert_eq!(
            shown("http://[::1]/", "::1"),
            " ([::1])",
            "a label cannot name it"
        );
    }

    #[test]
    fn only_the_visible_label_names_a_host_and_trailing_dots_are_trimmed() {
        for label in [
            "evil.test.\u{7}paypal.com",
            "evil.test\u{200B}.paypal.com",
            "evil.test.\u{AD}paypal.com",
        ] {
            assert_eq!(
                shown("https://evil.test/", label),
                " (evil.test)",
                "{label:?}"
            );
        }
        // A tab is painted as a space: it ends the token.
        assert_eq!(shown("https://evil.test/", "evil.test\tpaypal.com"), "");
        assert_eq!(shown("https://evil.test./", "evil.test"), "");
        assert_eq!(shown("https://www.example.com./", "docs"), " (example.com)");
        assert_eq!(shown("https://www./", ""), " (www)");
    }

    #[test]
    fn a_target_above_two_kib_is_not_linkable() {
        let url = |len: usize| format!("https://e.test/{}", "a".repeat(len - 15));
        assert!(safe_target(&url(2048)).is_some());
        assert_eq!(safe_target(&url(2049)), None);
        assert_eq!(safe_target(&url(3000)), None);
    }

    #[test]
    fn unsafe_destinations_have_no_safe_target() {
        for dest in [
            "javascript:alert(1)",
            "docs/readme.md",
            "mailto:a@b.test",
            "ftp://e.test/",
            "https://e.test/\u{1b}]8;;",
            "https://e.test/\u{7}",
            "https://e.test/\u{9c}",
            "https://e .test/",
            "",
        ] {
            assert_eq!(safe_target(dest), None, "{dest:?}");
        }
    }

    #[test]
    fn a_safe_target_never_carries_a_control_character() {
        for dest in [
            "https://e.test/%1b%5d8;;x%07",
            "https://e.test/?q=%C2%A0z",
            "https://E.TEST:443/Path?a=b#frag",
        ] {
            let safe = safe_target(dest).expect("safe");
            assert!(
                safe.chars().all(|c| !c.is_control() && !c.is_whitespace()),
                "{safe:?}"
            );
        }
    }
}
