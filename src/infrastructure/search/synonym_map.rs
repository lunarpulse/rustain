#![cfg(feature = "meta-search")]
//! Synonym map for BM25 query-side expansion (Story 9-7c, AC-9-7c-1).
//!
//! Loaded at module-init from `synonyms.toml` via `std::sync::LazyLock`.
//! Query-side expansion (NOT bm25 Tokenizer trait wrap) per AC-9-7c-2 rationale.

use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Error type for synonym map construction failures.
#[derive(Debug, thiserror::Error)]
pub enum SynonymMapError {
    #[error("invalid TOML: {0}")]
    InvalidToml(#[from] toml::de::Error),
    #[error("pair 'from' must be non-empty")]
    PairFromEmpty,
    #[error("pair 'to' must be non-empty")]
    PairToEmpty,
    #[error("fanout too large for '{from}': {len} > 5")]
    FanoutTooLarge { from: String, len: usize },
    #[error("self-loop detected: '{from}' appears in its own synonym closure")]
    SelfLoop { from: String },
    #[error("duplicate 'from' key: '{from}' already defined")]
    DuplicateFrom { from: String },
}

/// Parsed synonym pair from TOML.
#[derive(Debug, serde::Deserialize)]
struct TomlPair {
    from: String,
    to: Vec<String>,
    #[allow(dead_code)]
    justification: String,
}

/// Parsed TOML root.
#[derive(Debug, serde::Deserialize)]
struct TomlRoot {
    #[serde(rename = "pair")]
    pairs: Vec<TomlPair>,
}

/// Mary-curated synonym map.  Case-insensitive expansion with bounded fanout.
#[derive(Debug, Clone)]
pub struct SynonymMap {
    inner: BTreeMap<String, Vec<String>>,
}

impl SynonymMap {
    /// Empty synonym map (test injection for control group).
    pub fn empty() -> Self {
        Self {
            inner: BTreeMap::new(),
        }
    }

    /// Load from the compiled-in `synonyms.toml`.
    pub fn from_default_toml() -> Self {
        Self::from_toml_str(include_str!("synonyms.toml"))
            .expect("compiled-in synonyms.toml must be valid")
    }

    /// Load from an arbitrary TOML string (test injection).
    pub fn from_toml_str(toml: &str) -> Result<Self, SynonymMapError> {
        let root: TomlRoot = toml::from_str(toml)?;
        let mut inner = BTreeMap::new();

        for pair in root.pairs {
            let from_lower = pair.from.to_lowercase();
            if from_lower.is_empty() {
                return Err(SynonymMapError::PairFromEmpty);
            }
            if pair.to.is_empty() {
                return Err(SynonymMapError::PairToEmpty);
            }
            if pair.to.len() > 5 {
                return Err(SynonymMapError::FanoutTooLarge {
                    from: pair.from.clone(),
                    len: pair.to.len(),
                });
            }

            // Reject duplicate `from` keys — last-write-wins silently
            // discards the first entry's mapping.
            if inner.contains_key(&from_lower) {
                return Err(SynonymMapError::DuplicateFrom {
                    from: pair.from.clone(),
                });
            }

            // Deduplicate 'to' entries.
            let to_deduped: Vec<String> = {
                let mut seen = std::collections::BTreeSet::new();
                pair.to
                    .into_iter()
                    .map(|s| s.to_lowercase())
                    .filter(|s| seen.insert(s.clone()))
                    .collect()
            };

            // Self-loop check: from must NOT be in its own to list.
            if to_deduped.contains(&from_lower) {
                return Err(SynonymMapError::SelfLoop { from: pair.from });
            }

            inner.insert(from_lower, to_deduped);
        }

        Ok(Self { inner })
    }

    /// Case-insensitive expansion. Unknown tokens return an empty Vec.
    /// Guarantees:
    /// (a) no self-loop,
    /// (b) bounded fanout ≤ 5,
    /// (c) deduplicated result tokens.
    pub fn expand(&self, token: &str) -> Vec<String> {
        self.inner
            .get(&token.to_lowercase())
            .cloned()
            .unwrap_or_default()
    }

    /// Expand all tokens in a whitespace-split query string.
    /// Returns (`expanded_query`, `synonym_expansion_triggered: bool`).
    pub fn expand_query(&self, query: &str) -> (String, bool) {
        let mut expanded_parts = Vec::new();
        let mut triggered = false;

        for token in query.split_whitespace() {
            expanded_parts.push(token.to_string());
            let synonyms = self.expand(token);
            if !synonyms.is_empty() {
                triggered = true;
                for syn in synonyms {
                    if syn != token.to_lowercase() {
                        expanded_parts.push(syn);
                    }
                }
            }
        }

        // Deduplicate: when two input tokens both expand to the same synonym
        // (e.g. "neat tidy" → both expand to "format"), remove duplicates so
        // BM25 IDF/TF scoring is not skewed.
        let mut seen = std::collections::BTreeSet::new();
        expanded_parts.retain(|t| seen.insert(t.clone()));

        (expanded_parts.join(" "), triggered)
    }
}

/// Global synonym map loaded once at first access.
pub static SYNONYMS: LazyLock<SynonymMap> = LazyLock::new(SynonymMap::from_default_toml);

#[cfg(test)]
mod tests {
    use super::*;

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_loads_default_toml_with_5_to_8_pairs() {
        let map = SynonymMap::from_default_toml();
        let count = map.inner.len();
        assert!(
            (5..=8).contains(&count),
            "Expected 5–8 synonym pairs, got {}",
            count
        );
    }

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_expands_known_alias() {
        let map = SynonymMap::from_default_toml();
        let expanded = map.expand("neat");
        assert!(
            expanded.contains(&"format".to_string()),
            "'neat' should expand to 'format'"
        );
    }

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_is_case_insensitive() {
        let map = SynonymMap::from_default_toml();
        assert_eq!(map.expand("NEAT"), map.expand("neat"));
        assert_eq!(map.expand("NeAt"), map.expand("neat"));
    }

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_unknown_term_passthrough() {
        let map = SynonymMap::from_default_toml();
        assert!(map.expand("zzz_unknown").is_empty());
    }

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_no_self_loop() {
        let map = SynonymMap::from_default_toml();
        for (from, to) in &map.inner {
            for t in to {
                let expanded = map.expand(t);
                assert!(
                    !expanded.contains(from),
                    "Self-loop detected: '{}' in expand('{}')",
                    from,
                    t
                );
            }
        }
    }

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_no_duplicate_tokens() {
        let map = SynonymMap::from_default_toml();
        for from in map.inner.keys() {
            let expanded = map.expand(from);
            let set: std::collections::BTreeSet<_> = expanded.iter().collect();
            assert_eq!(
                set.len(),
                expanded.len(),
                "Duplicate tokens in expand('{}'): {:?}",
                from,
                expanded
            );
        }
    }

    /// AC: 9-7c-1
    #[test]
    fn test_synonym_map_fanout_bounded() {
        let map = SynonymMap::from_default_toml();
        for from in map.inner.keys() {
            let expanded = map.expand(from);
            assert!(
                expanded.len() <= 5,
                "Fanout for '{}' = {} > 5",
                from,
                expanded.len()
            );
        }
    }

    /// AC: 9-7c-1
    #[test]
    fn test_expand_query_reports_triggered_flag() {
        let map = SynonymMap::from_default_toml();
        let (_, triggered) = map.expand_query("hello neat world");
        assert!(triggered, "expand_query with 'neat' should trigger");

        let (_, triggered) = map.expand_query("hello world");
        assert!(
            !triggered,
            "expand_query without synonyms should not trigger"
        );
    }
}
