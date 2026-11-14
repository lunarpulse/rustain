//! The one `--version` block (FR109): build metadata from `build.rs`, protocol versions
//! from the constants the handshakes use, features from `cfg!`.
//!
//! Every surface that prints version facts (clap's `--version`, the palette's
//! `ShowVersion` notice, the crash reports) reads this module.

/// The compiler that built this binary (`build.rs`, `$RUSTC --version`).
pub const RUSTC_VERSION: &str = env!("RUSTC_VERSION");

/// One Cargo feature (every key of `[features]` except `default`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    pub name: &'static str,
    pub built: bool,
    /// Test-only features are listed when built, never under `not built:`.
    pub test_only: bool,
}

/// How a protocol appears on the `protocols:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Version(String),
    Unversioned,
    NotBuilt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol {
    pub name: &'static str,
    pub support: Support,
}

/// Each feature name is written once: the name and its `cfg!` cannot diverge.
macro_rules! features {
    ($($name:literal $(test_only = $test_only:literal)?),* $(,)?) => {
        &[$(Feature {
            name: $name,
            built: cfg!(feature = $name),
            test_only: false $(|| $test_only)?,
        }),*]
    };
}

/// Sorted by name; `tests/cli_version.rs` fails when this set and `Cargo.toml`
/// `[features]` (minus `default`) differ.
const FEATURES: &[Feature] = features![
    "a2a",
    "anthropic",
    "clipboard",
    "cron",
    "mcp",
    "meta-search",
    "models-dev",
    "ollama",
    "openai",
    "p2p",
    "p2p-test-utils" test_only = true,
    "relay-server",
    "sandbox",
    "self-update",
    "skills-validation",
    "telegram",
    "test-fake-mcp" test_only = true,
    "test-instrumentation" test_only = true,
    "vector-search",
    "wasm-sandbox",
];

pub fn features() -> &'static [Feature] {
    FEATURES
}

/// `agent-skills`, `acp`, `mcp`, `a2a`, in that order.
pub fn protocols() -> Vec<Protocol> {
    #[cfg(feature = "mcp")]
    let mcp = Support::Version(crate::adapters::mcp::client::offered_protocol_version());
    #[cfg(not(feature = "mcp"))]
    let mcp = Support::NotBuilt;
    #[cfg(feature = "a2a")]
    let a2a = Support::Version(crate::adapters::a2a::jsonrpc::A2A_PROTOCOL_VERSION.to_string());
    #[cfg(not(feature = "a2a"))]
    let a2a = Support::NotBuilt;
    vec![
        Protocol {
            name: "agent-skills",
            support: Support::Unversioned,
        },
        Protocol {
            name: "acp",
            support: Support::Version(
                crate::adapters::acp::agent::ACP_PROTOCOL_VERSION.to_string(),
            ),
        },
        Protocol {
            name: "mcp",
            support: mcp,
        },
        Protocol {
            name: "a2a",
            support: a2a,
        },
    ]
}

fn render_protocol(protocol: &Protocol) -> String {
    match &protocol.support {
        Support::Version(version) => format!("{} {version}", protocol.name),
        Support::Unversioned => protocol.name.to_string(),
        Support::NotBuilt => format!("{} not built", protocol.name),
    }
}

/// Comma-separated names, `none` for an empty list.
fn render_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let joined = names.collect::<Vec<_>>().join(", ");
    if joined.is_empty() {
        "none".to_string()
    } else {
        joined
    }
}

/// The block without the leading `rustain `: clap prints `{command name} {long_version}`.
pub fn long_version() -> String {
    let target = option_env!("TARGET").unwrap_or("unknown");
    let build_date = option_env!("BUILD_DATE").unwrap_or("unknown");
    let git_hash = option_env!("GIT_HASH").unwrap_or("unknown");
    let protocols = protocols()
        .iter()
        .map(render_protocol)
        .collect::<Vec<_>>()
        .join(", ");
    let built = render_list(features().iter().filter(|f| f.built).map(|f| f.name));
    let not_built = render_list(
        features()
            .iter()
            .filter(|f| !f.built && !f.test_only)
            .map(|f| f.name),
    );
    format!(
        "{version} ({target})\nbuild: {build_date}\ncommit: {git_hash}\nrustc: {RUSTC_VERSION}\nprotocols: {protocols}\nfeatures: {built}\nnot built: {not_built}",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// The full block, first line `rustain <version> (<target>)`.
// Covers: FR109
pub fn version_block() -> String {
    format!("rustain {}", long_version())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(block: &'a str, label: &str) -> &'a str {
        block
            .lines()
            .find_map(|l| l.strip_prefix(label))
            .unwrap_or_else(|| panic!("no `{label}` line in {block}"))
    }

    #[test]
    fn version_block_is_long_version_with_the_program_name() {
        assert_eq!(version_block(), format!("rustain {}", long_version()));
        assert!(version_block().starts_with(&format!("rustain {} (", env!("CARGO_PKG_VERSION"))));
        assert!(!long_version().starts_with("rustain"));
    }

    #[test]
    fn labels_appear_once_in_the_fixed_order() {
        let block = version_block();
        let labels: Vec<&str> = block
            .lines()
            .skip(1)
            .map(|l| l.split_once(": ").expect("label: value").0)
            .collect();
        assert_eq!(
            labels,
            [
                "build",
                "commit",
                "rustc",
                "protocols",
                "features",
                "not built"
            ]
        );
    }

    /// The `build:` line is `unknown` or an ISO-8601 UTC timestamp (`DDDD-DD-DDTDD:DD:DDZ`).
    #[test]
    fn build_line_is_a_utc_timestamp_or_unknown() {
        let block = version_block();
        let build = line(&block, "build: ");
        let shaped = build.len() == 20
            && build.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 => b == b'-',
                10 => b == b'T',
                13 | 16 => b == b':',
                19 => b == b'Z',
                _ => b.is_ascii_digit(),
            });
        assert!(
            build == "unknown" || shaped,
            "unexpected build date: {build:?}"
        );
    }

    #[test]
    fn empty_list_renders_none() {
        assert_eq!(render_list(std::iter::empty()), "none");
        assert_eq!(render_list(["a", "b"].into_iter()), "a, b");
    }

    #[test]
    fn features_are_sorted_and_unique() {
        let names: Vec<&str> = features().iter().map(|f| f.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn support_renders_each_variant() {
        let p = |support| Protocol { name: "x", support };
        assert_eq!(render_protocol(&p(Support::Unversioned)), "x");
        assert_eq!(render_protocol(&p(Support::NotBuilt)), "x not built");
        assert_eq!(render_protocol(&p(Support::Version("2".into()))), "x 2");
    }

    #[test]
    fn protocols_are_listed_in_the_fixed_order() {
        let names: Vec<&str> = protocols().iter().map(|p| p.name).collect();
        assert_eq!(names, ["agent-skills", "acp", "mcp", "a2a"]);
    }
}
