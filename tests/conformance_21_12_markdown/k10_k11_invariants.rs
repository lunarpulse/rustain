//! K10 — height truth and the scoped AD-2112 gate; K11 — robustness (Story
//! 21.12, AC8/AC9; baseline B12, B13, B19).
//!
//! K10: `compute_height == render().lines.len()` for every generated source in
//! every mode with the dark and the monochrome theme; one pipeline run per
//! open-prose draw on both hosts; a text gate over the markdown sources.
//! K11: a deterministic proptest suite plus adversarial inputs that must finish
//! on a default-size thread stack in bounded time.

use super::support::*;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use rustain::adapters::tui::markdown::{self, RenderOptions, metrics};
use rustain::adapters::tui::theme::Theme;
use rustain::infrastructure::terminal_info::ColorCapability;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const G: &str = "👨\u{200d}👩\u{200d}👧";
const C: &str = "界";
const A: &str = "e\u{301}";

const FRAGMENTS: &[&str] = &[
    "# Heading",
    "## Sub heading",
    "###### six",
    "text with **bold** and *em* and ~~gone~~ and `code`",
    "`a  b`",
    "- item",
    "  - nested",
    "    - deeper",
    "1. one",
    "3. three",
    "- [ ] todo",
    "- [x] done",
    "| Name | Qty |\n|:-----|----:|\n| `a  b` | **12** |\n| 界界 | e\u{301} |",
    "| ctl\u{1b}]52;c;eA==\u{7} | \u{9b}31m c1 |\n|:-:|--:|\n| bidi \u{202e}x\u{200b}y | a\tb |\n| 👨\u{200d}👩\u{200d}👧 | z\u{0}w |",
    "> quote",
    "> > deeper quote",
    "---",
    "```rust\nfn f() {}\n```",
    "```\n\tTabbed\tcode\n",
    "~~~\nunclosed tilde",
    "````md\n```\ninner\n```\n````",
    "<div>x</div>",
    "<!-- open comment",
    "Vec<String> and Option<T>",
    "SELECT * FROM t",
    "2 \\* 3",
    "**unclosed",
    "*em*text_with_under_scores",
    "[label](https://example.test/path) <https://auto.test/>",
    "[bad](javascript:alert(1)) ![alt text](u.png) ![](x.png)",
    "https://very.long.example.test/path/that/keeps/going/and/going/and/going/forever/and/ever",
    "界界界界界界界界界界",
    "👨\u{200d}👩\u{200d}👧👨\u{200d}👩\u{200d}👧",
    "e\u{301}e\u{301}e\u{301}",
    "a\tb\tc",
    "ctl\u{7}\u{1b}[31m\u{8}x",
    "```\u{1b}]52;c;eA==\u{7}\nx\n```",
    "x  y   z",
    "| a | b |\n|---|---|\n| 1 | 2 | 3 |",
    "hard  \nbreak\\\nhere",
    "| `a | b` | **x |\n|---|---|\n| y | `z |",
    "| a |\n|---|\n| 1 | 2 | 3 | 4 |",
    "| 名前 | n |\n|---|---|\n| 界界界界界界界界 | 1 |",
];
const SEPARATORS: &[&str] = &["\n", "\n\n", " ", "", "  \n", "\r\n"];

fn source_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec((0..FRAGMENTS.len(), 0..SEPARATORS.len()), 1..9).prop_map(|parts| {
        parts
            .iter()
            .map(|(f, s)| format!("{}{}", FRAGMENTS[*f], SEPARATORS[*s]))
            .collect()
    })
}

fn opts(index: usize) -> RenderOptions {
    match index {
        0 => RenderOptions::streaming(),
        1 => RenderOptions::completed(),
        _ => RenderOptions::literal(),
    }
}

fn theme(mono: bool) -> Theme {
    if mono {
        Theme::for_capability(ColorCapability::Monochrome)
    } else {
        Theme::dark()
    }
}

fn check(source: &str, width: usize, mode: usize, mono: bool) -> Result<(), TestCaseError> {
    let theme = theme(mono);
    let opts = opts(mode);
    let rendered = markdown::render(source, width, &theme, &opts);
    prop_assert_eq!(
        rendered.lines.len(),
        markdown::compute_height(source, width, &theme, &opts),
        "height != lines for {:?} width {} mode {}",
        source,
        width,
        mode
    );
    if width == 0 {
        prop_assert!(rendered.lines.is_empty());
    }
    for line in &rendered.lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        for span in &line.spans {
            prop_assert!(
                !span.content.chars().any(char::is_control),
                "control character in {:?}",
                span.content
            );
        }
        // The sole exception is one grapheme wider than the whole width.
        prop_assert!(
            text.width() <= width || text.graphemes(true).count() == 1,
            "line {:?} is {} cells at width {} (source {:?}, mode {})",
            text,
            text.width(),
            width,
            source,
            mode
        );
    }
    Ok(())
}

fn deterministic_runner(cases: u32) -> TestRunner {
    TestRunner::new_with_rng(
        Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    )
}

#[test]
fn k10_height_equals_rendered_lines_for_generated_markdown_in_every_mode_and_theme() {
    let strategy = (source_strategy(), 0usize..=130, 0usize..3, any::<bool>());
    deterministic_runner(384)
        .run(&strategy, |(source, width, mode, mono)| {
            check(&source, width, mode, mono)
        })
        .unwrap();
}

#[test]
fn k10_every_fragment_holds_the_invariants_at_the_stage_widths() {
    for fragment in FRAGMENTS {
        for width in [0usize, 1, 2, 3, 7, 40, 80, 120] {
            for mode in 0..3 {
                for mono in [false, true] {
                    check(fragment, width, mode, mono)
                        .unwrap_or_else(|e| panic!("{fragment:?} @ {width}: {e}"));
                }
            }
        }
    }
    // The 21-11 C/G/A fixtures survive every width without a panic.
    for fixture in [C.repeat(12), G.repeat(9), A.repeat(30)] {
        for width in 0..=40 {
            for mode in 0..3 {
                check(&fixture, width, mode, false).unwrap();
            }
        }
    }
}

#[test]
fn k10_open_prose_is_rendered_once_per_draw_on_both_hosts() {
    let theme = Theme::dark();
    let text = "# Head\n\nstreaming **bold te and a long enough line to wrap around the pane";

    metrics::reset();
    let open = crate::render_helpers::make_turn("open", vec![], None);
    let local = paint_local(
        &crate::render_helpers::make_conversation(vec![], vec![]),
        Some(&open),
        &Default::default(),
        Some(text),
        &theme,
        40,
        30,
    );
    assert_eq!(metrics::renders(), 1, "local open prose: one pipeline run");
    assert!(
        rows(&local.buffer).iter().any(|r| r.contains("bold te")),
        "the open prose must actually be painted"
    );

    metrics::reset();
    let attached = paint_attached(
        &crate::render_helpers::make_conversation(vec![], vec![]),
        &streaming_buffer(text),
        &theme,
        40,
        30,
    );
    assert_eq!(
        metrics::renders(),
        1,
        "attached open prose: one pipeline run"
    );
    assert!(rows(&attached.buffer).iter().any(|r| r.contains("bold te")));

    // Positive control: the counter counts, so a second pass would show.
    metrics::reset();
    let _ = markdown::compute_height(text, 38, &theme, &RenderOptions::streaming());
    let _ = markdown::render(text, 38, &theme, &RenderOptions::streaming());
    assert_eq!(metrics::renders(), 2);
}

#[test]
fn k10_pane_height_matches_painted_rows_for_every_fixture_on_both_themes() {
    for mono in [false, true] {
        let theme = theme(mono);
        for fragment in FRAGMENTS.iter().take(26) {
            let pane = paint_local(
                &turn_conversation(fragment),
                None,
                &Default::default(),
                None,
                &theme,
                42,
                200,
            );
            let expected =
                markdown::compute_height(fragment, 40, &theme, &RenderOptions::completed());
            // One gutter half-line follows the markdown rows. Every painted
            // row of the turn carries the `│` gutter, so the rows in the
            // buffer are counted, not read back from the pane's own height.
            let painted = rows(&pane.buffer)
                .iter()
                .filter(|row| row.starts_with('│'))
                .count();
            assert_eq!(
                painted,
                expected + usize::from(expected > 0),
                "{fragment:?} mono={mono}"
            );
            assert_eq!(pane.total_height, painted, "{fragment:?} mono={mono}");
        }
    }
}

// ── Scoped AD-2112 gate ──────────────────────────────────────────────────────

const MARKDOWN_SOURCES: &[(&str, &str)] = &[
    (
        "mod.rs",
        include_str!("../../src/adapters/tui/markdown/mod.rs"),
    ),
    (
        "parse.rs",
        include_str!("../../src/adapters/tui/markdown/parse.rs"),
    ),
    (
        "transform.rs",
        include_str!("../../src/adapters/tui/markdown/transform.rs"),
    ),
    (
        "layout.rs",
        include_str!("../../src/adapters/tui/markdown/layout.rs"),
    ),
    (
        "sanitize.rs",
        include_str!("../../src/adapters/tui/markdown/sanitize.rs"),
    ),
    (
        "table.rs",
        include_str!("../../src/adapters/tui/markdown/table.rs"),
    ),
    (
        "highlight.rs",
        include_str!("../../src/adapters/tui/markdown/highlight.rs"),
    ),
];

/// The source before its trailing unit-test module.
fn production_part(source: &str) -> &str {
    source
        .find("\n#[cfg(test)]\nmod ")
        .map_or(source, |at| &source[..at])
}

/// Body of `fn name` (first `{` to its matching `}`); strings and comments in
/// the gated functions contain no braces.
fn function_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let start = source.find(&format!("fn {name}("))?;
    let open = start + source[start..].find('{')?;
    let mut depth = 0usize;
    for (offset, c) in source[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[open..=open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Gate violations over `(file, source)` pairs: the baseline defects that must
/// not return.
fn gate_violations(sources: &[(&str, &str)]) -> Vec<String> {
    let mut found = Vec::new();
    for (file, source) in sources {
        let production = production_part(source);
        for banned in ["strip_html", "strip_unclosed_markers"] {
            if production.contains(banned) {
                found.push(format!("{file}: `{banned}` is back (D2)"));
            }
        }
        if *file == "parse.rs" && production.contains(".min(1)") {
            found.push(format!("{file}: `.min(1)` list depth cap is back (B1)"));
        }
        if *file == "mod.rs"
            && let Some(body) = function_body(production, "compute_height")
        {
            for banned in ["Theme::", "RenderOptions::"] {
                if body.contains(banned) {
                    found.push(format!(
                        "{file}: `{banned}` constructed inside compute_height (AD-2113)"
                    ));
                }
            }
        }
    }
    found
}

fn src_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            src_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `(relative path, text)` pairs of every Rust file under `src/`.
fn src_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    src_files(&root, &mut files);
    files
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, std::fs::read_to_string(&path).unwrap())
        })
        .collect()
}

/// Relative paths of the sources outside the markdown module and `allowed`
/// that mention the parser crate.
fn private_parser_users(sources: &[(String, String)], allowed: &[&str]) -> Vec<String> {
    sources
        .iter()
        .filter(|(rel, text)| {
            !rel.starts_with("adapters/tui/markdown/")
                && !allowed.contains(&rel.as_str())
                && text.contains("pulldown_cmark")
        })
        .map(|(rel, _)| rel.clone())
        .collect()
}

#[test]
fn k10_gate_baseline_defects_stay_out_of_the_markdown_sources() {
    let violations = gate_violations(MARKDOWN_SOURCES);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
    assert_eq!(MARKDOWN_SOURCES.len(), 7, "the gate must stay scoped");
    // 12-B allowlists exactly one domain module: the producer-side helper.
    let users = private_parser_users(&src_sources(), &["domain/services/markdown_source.rs"]);
    assert!(
        users.is_empty(),
        "pulldown_cmark must be imported only under src/adapters/tui/markdown/: {users:?}"
    );
}

#[test]
fn k10_gate_positive_controls_see_what_they_ban() {
    let bad_mod = "pub fn compute_height(c: &str) -> usize { let t = Theme::dark(); let o = RenderOptions::completed(); 0 }";
    let bad_parse = "let depth = list_stack.len().min(1); fn strip_html() {}";
    let found = gate_violations(&[("mod.rs", bad_mod), ("parse.rs", bad_parse)]);
    assert!(found.iter().any(|v| v.contains("Theme::")), "{found:?}");
    assert!(
        found.iter().any(|v| v.contains("RenderOptions::")),
        "{found:?}"
    );
    assert!(found.iter().any(|v| v.contains(".min(1)")), "{found:?}");
    assert!(found.iter().any(|v| v.contains("strip_html")), "{found:?}");
    // Test modules and unrelated functions are not gated.
    let clean = "pub fn compute_height(c: &str) -> usize { c.len() }\nfn other() { Theme::dark(); }\n#[cfg(test)]\nmod tests { fn strip_html() {} }";
    assert!(gate_violations(&[("mod.rs", clean)]).is_empty());
    // The allowlist works: an allowed file that mentions the crate drops out
    // of the result while another offender stays.
    let files = [
        (
            "adapters/tui/markdown/parse.rs",
            "use pulldown_cmark::Parser;",
        ),
        ("adapters/other/allowed.rs", "use pulldown_cmark::Parser;"),
        ("adapters/other/offender.rs", "use pulldown_cmark::Parser;"),
        ("adapters/other/clean.rs", "fn clean() {}"),
    ]
    .map(|(rel, text)| (rel.to_owned(), text.to_owned()));
    assert_eq!(
        private_parser_users(&files, &[]),
        ["adapters/other/allowed.rs", "adapters/other/offender.rs"]
    );
    assert_eq!(
        private_parser_users(&files, &["adapters/other/allowed.rs"]),
        ["adapters/other/offender.rs"]
    );
}

// ── K11 adversarial inputs ───────────────────────────────────────────────────

/// Run `work` on a default-size (2 MiB) thread stack and require it to finish
/// within a bound: a stack overflow aborts the process, a hang fails here.
fn bounded<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(work());
        })
        .unwrap();
    match rx.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => {
            panic!("adversarial render did not finish within 120 s (hang or quadratic work)")
        }
        // The sender is dropped without a value only when `work` panicked.
        Err(RecvTimeoutError::Disconnected) => {
            let payload = worker.join().expect_err("a dropped sender means a panic");
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic payload>");
            panic!("adversarial render panicked: {message}")
        }
    }
}

fn assert_fits(lines: &[ratatui::text::Line<'_>], width: usize) {
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.width() <= width, "{text:?} exceeds {width}");
    }
}

/// The alphanumeric characters painted in `lines`, in order (prefixes and
/// spaces dropped; rows may split a word).
fn flat_text(lines: &[ratatui::text::Line<'_>]) -> String {
    lines
        .iter()
        .flat_map(|l| l.spans.iter().flat_map(|s| s.content.chars()))
        .filter(|c| c.is_alphanumeric())
        .collect()
}

#[test]
fn k11_ten_thousand_nested_quotes_keep_bounded_prefixes() {
    let source = format!("{}deep text", "> ".repeat(10_000));
    for width in [10usize, 40, 80] {
        let source = source.clone();
        let lines = bounded(move || {
            markdown::render(&source, width, &Theme::dark(), &RenderOptions::completed()).lines
        });
        assert_fits(&lines, width);
        assert!(
            flat_text(&lines).contains("deeptext"),
            "content survives at width {width}"
        );
    }
}

#[test]
fn k11_ten_thousand_deep_lists_keep_bounded_prefixes() {
    let source = format!("{}leaf", "- ".repeat(10_000));
    for width in [10usize, 80] {
        let source = source.clone();
        let lines = bounded(move || {
            markdown::render(&source, width, &Theme::dark(), &RenderOptions::completed()).lines
        });
        assert_fits(&lines, width);
        assert!(flat_text(&lines).contains("leaf"));
    }
}

#[test]
fn k11_one_mebibyte_single_line_renders_in_every_mode() {
    let token = "a".repeat(1 << 20);
    let words = "ab ".repeat((1 << 20) / 3);
    for source in [token, words] {
        for mode in 0..3 {
            let source = source.clone();
            let lines =
                bounded(move || markdown::render(&source, 80, &Theme::dark(), &opts(mode)).lines);
            assert_fits(&lines, 80);
            assert!(lines.len() > 1000);
        }
    }
}

#[test]
fn k11_unclosed_delimiters_and_fences_stream_without_panicking() {
    let mut source = String::new();
    for i in 0..4000 {
        source.push_str(&format!("*a{i} **b `c ~~d _e "));
    }
    let lines = bounded(move || {
        markdown::render(&source, 60, &Theme::dark(), &RenderOptions::streaming()).lines
    });
    assert_fits(&lines, 60);
}

#[test]
fn k11_streaming_marker_openers_and_closers_finish_in_bounded_time() {
    // `*` openers followed by `_` closers whose character never matches an open
    // marker: the streaming closer scan walks the whole stack for each closer.
    // Measured (debug, loaded host) the work is QUADRATIC, in the sanitizer and
    // in pulldown-cmark itself: 10k+10k 2.7 s, 20k+20k 9 s, 40k+40k 36 s through
    // `render`; the sanitizer alone 0.39 s / 1.46 s at 10k / 20k. The 100k+100k
    // input (about 4 minutes) is therefore not run in CI: the full path runs
    // at 10k and the sanitizer alone at 50k, each under a generous bound that
    // only catches a hang or a further blow-up. DF-21-12-STREAM-CLOSER-BLOCK-AWARENESS.
    let source = format!("{}{}", " *a".repeat(10_000), "a_ ".repeat(10_000));
    let started = std::time::Instant::now();
    let lines = bounded(move || {
        markdown::render(&source, 80, &Theme::dark(), &RenderOptions::streaming()).lines
    });
    let elapsed = started.elapsed();
    assert_fits(&lines, 80);
    assert!(!lines.is_empty());
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "streaming 10k openers + 10k closers took {elapsed:?}"
    );

    let source = format!("{}{}", " *a".repeat(50_000), "a_ ".repeat(50_000));
    let started = std::time::Instant::now();
    let sanitized = bounded(move || {
        markdown::sanitize::sanitize(&source, markdown::RenderMode::Streaming).into_owned()
    });
    assert!(sanitized.contains("a_ "), "closers stay in the text");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "sanitizing 50k openers + 50k closers took {:?}",
        started.elapsed()
    );
}

#[test]
fn k11_five_thousand_table_columns_collapse_in_bounded_time() {
    let header = format!("|{}\n", "c|".repeat(5_000));
    let delimiter = format!("|{}\n", "---|".repeat(5_000));
    let body = format!("|{}\n", "x|".repeat(5_001));
    let source = format!("{header}{delimiter}{body}");
    for (width, mode) in [
        (0usize, 1usize),
        (1, 1),
        (10, 0),
        (80, 1),
        (80, 0),
        (120, 2),
    ] {
        let source = source.clone();
        let lines =
            bounded(move || markdown::render(&source, width, &Theme::dark(), &opts(mode)).lines);
        assert_fits(&lines, width);
        if width == 80 && mode != 2 {
            let text: Vec<String> = lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            // The repair adds the column the 5,001-cell body row needs.
            assert_eq!(text, ["table · 5001 cols × 1 rows", ""]);
        }
    }
}

#[test]
fn k11_repeated_delimiter_lines_sanitize_in_bounded_time() {
    // A table whose body is no wider than its header needs no repair; the
    // 50,000 delimiter-like body rows after it are body rows, never new header
    // candidates (each retry would rescan every remaining row: O(N^2)).
    let source = "|-|\n".repeat(50_000);
    for mode in [
        markdown::RenderMode::Completed,
        markdown::RenderMode::Streaming,
    ] {
        let input = source.clone();
        let started = std::time::Instant::now();
        let sanitized = bounded(move || markdown::sanitize::sanitize(&input, mode).into_owned());
        let elapsed = started.elapsed();
        assert_eq!(sanitized, source, "no repair is needed in {mode:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "sanitizing 50k delimiter lines took {elapsed:?} in {mode:?}"
        );
    }
    for options in [RenderOptions::completed(), RenderOptions::streaming()] {
        let input = source.clone();
        let lines = bounded(move || markdown::render(&input, 40, &Theme::dark(), &options).lines);
        assert_fits(&lines, 40);
    }
}

#[test]
fn k11_a_huge_table_and_a_huge_cell_render_in_bounded_time() {
    let rows: String = (0..10_000).map(|i| format!("| r{i} | v |\n")).collect();
    let table = format!("| k | v |\n|---|---|\n{rows}");
    let cell = format!("| a |\n|---|\n| {} |\n", "界".repeat(1 << 19));
    let (table_lines, cell_lines) = bounded(move || {
        let theme = Theme::dark();
        (
            markdown::render(&table, 80, &theme, &RenderOptions::completed()).lines,
            markdown::render(&cell, 40, &theme, &RenderOptions::completed()).lines,
        )
    });
    assert_eq!(table_lines.len(), 10_000 + 5);
    assert_fits(&table_lines, 80);
    assert_fits(&cell_lines, 40);
    assert!(
        cell_lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('…')))
    );
}
