//! DF-075 measurement body (Story 21.12, AC10, Task 9), called by the
//! `#[ignore]`d test `k_measure_stream_cost` in the conformance root so CI
//! never runs it. Run once with
//!
//! ```sh
//! cargo test --test tui conformance_21_12_markdown::k_measure_stream_cost -- --ignored --nocapture
//! ```
//!
//! It prints the per-draw cost of the uncached markdown work on the **test
//! (debug) profile**, which over-estimates the shipped release build, and
//! asserts nothing: the decision rule (5 ms per draw at 32 KiB) is applied by
//! the developer to the printed numbers and recorded in the story.

use std::time::{Duration, Instant};

use rustain::adapters::tui::markdown::{
    self, LinkPresentation, RenderOptions, highlight, layout, parse, sanitize, transform,
};
use rustain::adapters::tui::theme::Theme;

/// The Stage's draw path: host suffix and OSC 8 runs (the widest presentation).
const STAGE: LinkPresentation = LinkPresentation::Labeled { osc8: true };

const WIDTH: usize = 78;
const SIZES_KIB: [usize; 4] = [2, 8, 32, 128];
const WARMUPS: usize = 3;
const RUNS: usize = 30;

/// One deterministic LLM-shaped section (paragraphs, nested lists, code, a
/// table, links, a quote, a heading).
fn section(n: usize) -> String {
    format!(
        "## Step {n}: reading the module\n\n\
         The loader in `src/loader.rs` reads **{n} files** and keeps a *small* cache; \
         when the cache is cold it falls back to the slow path described in \
         [the design notes](https://example.test/design/{n}) and logs a warning that is \
         long enough to wrap across more than one row at the default width.\n\n\
         - collect the inputs\n  - sort them by name\n    - then by size {n}\n  - drop duplicates\n\
         - hand them to the planner\n\n\
         1. parse the header\n2. validate the checksum\n3. map the sections\n\n\
         ```rust\nfn load(path: &Path) -> Result<Module> {{\n    let bytes = fs::read(path)?;\n    Module::parse(&bytes)\n}}\n```\n\n\
         | file | lines | status |\n|------|------:|--------|\n| a.rs | {n}0 | ok |\n| b.rs | {n}1 | slow |\n\n\
         > Note: the numbers above are illustrative for section {n}.\n\n"
    )
}

fn fixture(kib: usize) -> String {
    let mut text = String::new();
    let mut n = 0;
    while text.len() < kib * 1024 {
        n += 1;
        text.push_str(&section(n));
    }
    text
}

struct Stats {
    median: Duration,
    p95: Duration,
}

fn measure(mut work: impl FnMut()) -> Stats {
    for _ in 0..WARMUPS {
        work();
    }
    let mut samples: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            work();
            start.elapsed()
        })
        .collect();
    samples.sort();
    Stats {
        median: samples[RUNS / 2],
        p95: samples[(RUNS * 95).div_ceil(100) - 1],
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

pub fn run() {
    let theme = Theme::dark();
    println!(
        "host load before: {}",
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_else(|_| "unavailable".to_owned())
            .trim()
    );
    println!("profile: test (debug); width {WIDTH}; {WARMUPS} warm-ups, {RUNS} runs; ms");
    println!("(a) open streaming prose, one full pass (Streaming)");
    for kib in SIZES_KIB {
        let text = fixture(kib);
        let full = measure(|| {
            let _ = markdown::render(
                &text,
                WIDTH,
                &theme,
                &RenderOptions::streaming().with_links(STAGE),
            );
        });
        // Per-stage split of the same pass.
        let sanitize_ms = measure(|| {
            let _ = sanitize::sanitize(&text, markdown::RenderMode::Streaming);
        });
        let sanitized = sanitize::sanitize(&text, markdown::RenderMode::Streaming).into_owned();
        let parse_ms = measure(|| {
            let _ = parse::parse(&sanitized, STAGE);
        });
        let blocks = || parse::parse(&sanitized, STAGE);
        let transform_ms = measure(|| {
            let _ = transform::transform(blocks(), &theme);
        });
        let styled = || transform::transform(blocks(), &theme);
        let layout_ms = measure(|| {
            let _ = layout::layout(highlight::highlight(styled(), &theme), WIDTH, &theme);
        });
        println!(
            "  {kib:>3} KiB: median {:>8.2} p95 {:>8.2} | sanitize {:>7.2} | parse {:>7.2} | \
             parse+transform {:>7.2} | parse+transform+layout {:>7.2} (cumulative medians)",
            ms(full.median),
            ms(full.p95),
            ms(sanitize_ms.median),
            ms(parse_ms.median),
            ms(transform_ms.median),
            ms(layout_ms.median),
        );
    }
    println!("(b) re-rendering one visible committed prose part (Completed)");
    for kib in SIZES_KIB {
        let text = fixture(kib);
        let stats = measure(|| {
            let _ = markdown::render(
                &text,
                WIDTH,
                &theme,
                &RenderOptions::completed().with_links(STAGE),
            );
        });
        println!(
            "  {kib:>3} KiB: median {:>8.2} p95 {:>8.2}",
            ms(stats.median),
            ms(stats.p95)
        );
    }
    println!("(c) cold height pass over a 50-turn conversation (resize), 2 KiB per turn");
    let turns: Vec<String> = (0..50).map(|_| fixture(2)).collect();
    let stats = measure(|| {
        for turn in &turns {
            let _ = markdown::compute_height(
                turn,
                WIDTH,
                &theme,
                &RenderOptions::completed().with_links(STAGE),
            );
        }
    });
    println!(
        "  50 turns: median {:>8.2} p95 {:>8.2}",
        ms(stats.median),
        ms(stats.p95)
    );
    println!(
        "host load after: {}",
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_else(|_| "unavailable".to_owned())
            .trim()
    );
}
