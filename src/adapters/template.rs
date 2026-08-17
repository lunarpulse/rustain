//! Single-pass `{placeholder}` substitution — the one substitution engine every
//! shipped unit/plist template goes through.
//!
//! # Why this is its own module (Story 18.4c-b AC5)
//!
//! It was `daemon::service::render_template` (Story 12-1d AC-12-1d-5), and that
//! module is `#![cfg(unix)]`. `rustain relay serve --print-service-unit` writes
//! text to stdout and installs nothing, so there is no OS-specific behaviour to
//! gate: an operator on macOS may legitimately want the unit to copy onto a
//! Linux host. Leaving the renderer inside a `cfg(unix)` module would have made
//! the verb silently Unix-only, so the engine was lifted here **un-gated**
//! rather than duplicated.
//!
//! ⛔ **Do not copy this function.** The single-pass property is load-bearing: a
//! sequential-`.replace()` implementation re-expands a substituted VALUE that
//! happens to contain another placeholder, which is the template-injection
//! regression `daemon::service`'s
//! `render_template_does_not_re_expand_substituted_values` exists to catch. Two
//! copies is how one of them drifts back.

/// Substitute `{key}` occurrences in `template`, scanning once left-to-right.
///
/// An UNKNOWN `{...}` is emitted literally, so a template may contain braces
/// that are not ours. Because the scan never re-examines emitted output, a
/// substituted value that itself contains `{some_key}` is **not** re-expanded.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(close_rel) = after.find('}') {
            let key = &after[1..close_rel];
            match vars.iter().find(|(k, _)| *k == key) {
                Some((_, v)) => out.push_str(v),
                // Unknown placeholder: emit literally (keeps `{...}` that isn't ours).
                None => out.push_str(&after[..=close_rel]),
            }
            rest = &after[close_rel + 1..];
        } else {
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_substituted_value_carrying_a_placeholder_is_not_re_expanded() {
        // The property the whole module exists for, asserted where the engine
        // now lives as well as at its `daemon::service` caller.
        let out = render_template("A={one} B={two}", &[("one", "{two}-literal"), ("two", "SECOND")]);
        assert_eq!(out, "A={two}-literal B=SECOND");
    }

    #[test]
    fn an_unknown_placeholder_survives_verbatim() {
        assert_eq!(render_template("x={nope}", &[("yes", "1")]), "x={nope}");
    }

    #[test]
    fn an_unclosed_brace_terminates_the_scan_without_panicking() {
        assert_eq!(render_template("a {b", &[("b", "B")]), "a {b");
    }
}
