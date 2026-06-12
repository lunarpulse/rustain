#![allow(dead_code)] // shared test-support module; helpers used by a subset of integration-test binaries
//! Eval query set partition selector (Story 9-7c, AC-9-7c-3).
//!
//! ## Stratified split algorithm
//!
//! 1. Group queries by `category`.
//! 2. Shuffle each group with a seeded RNG (`rand::rngs::StdRng::seed_from_u64(42)`).
//! 3. Take `floor(group.len() * dev_ratio)` to dev, remainder to holdout.
//! 4. Preserve original order within each partition (post-shuffle order).
//!
//! ## Partition selector
//!
//! The harness has no `bin/eval_harness.rs` — the SCP's referenced `--partition`
//! flag is realized as the `RUSTAIN_EVAL_PARTITION=dev|holdout|all` env var
//! consumed inside the test functions.  Casual `cargo test --features meta-search`
//! defaults to `dev` so holdout is never touched accidentally.

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// AC: 9-7c-3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    Dev,
    Holdout,
    All,
}

impl Partition {
    /// Reads `RUSTAIN_EVAL_PARTITION`; defaults to `Dev` (NOT `All`) so a
    /// casual `cargo test --features meta-search` does not accidentally touch
    /// holdout.  Returns `Err` if the env var has an unrecognized value.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("RUSTAIN_EVAL_PARTITION").as_deref() {
            Ok("dev") => Ok(Partition::Dev),
            Ok("holdout") => Ok(Partition::Holdout),
            Ok("all") => Ok(Partition::All),
            Ok(other) => Err(format!(
                "Unrecognized RUSTAIN_EVAL_PARTITION='{}'. Expected: dev|holdout|all",
                other
            )),
            Err(_) => Ok(Partition::Dev),
        }
    }
}

/// AC: 9-7c-3 — stratified 70/30 split by category, deterministic under seed 42.
///
/// # Panics
///
/// Panics if `dev_ratio` is NaN, negative, infinite, or not in (0.0, 1.0).
pub fn stratified_split<T: Clone>(
    queries: Vec<T>,
    category_fn: impl Fn(&T) -> String,
    seed: u64,
    dev_ratio: f64,
) -> (Vec<T>, Vec<T>) {
    assert!(
        dev_ratio.is_finite() && dev_ratio > 0.0 && dev_ratio < 1.0,
        "stratified_split: dev_ratio must be finite and in (0.0, 1.0), got {}",
        dev_ratio
    );
    let mut by_cat: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for q in queries {
        let cat = category_fn(&q);
        by_cat.entry(cat).or_default().push(q);
    }

    let mut rng = StdRng::seed_from_u64(seed);
    let (mut dev, mut holdout) = (Vec::new(), Vec::new());

    for (_cat, mut group) in by_cat {
        group.shuffle(&mut rng);
        let split_at = (group.len() as f64 * dev_ratio).floor() as usize;
        // Guard: singleton category with a floor split result of 0 routes
        // the entire category to holdout by construction. Accept this as
        // an inherent property of stratified small-N splits.
        for (i, q) in group.into_iter().enumerate() {
            if i < split_at {
                dev.push(q);
            } else {
                holdout.push(q);
            }
        }
    }

    (dev, holdout)
}

/// AC: 9-7c-3 — load the appropriate partition file.
pub fn load_query_set_partitioned<Q>(p: Partition) -> Vec<Q>
where
    Q: for<'de> Deserialize<'de>,
    Q: Serialize,
{
    let path = match p {
        Partition::Dev => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/skill_eval_queries-dev.json"),
        Partition::Holdout => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/skill_eval_queries-holdout.json"),
        Partition::All => {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill_eval_queries.json")
        }
    };

    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read query set at {:?}: {}", path, e));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("Failed to parse query set: {}", e))
}

/// Compute SHA-256 of a file, return lower-case hex.
pub fn sha256_of_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("Cannot read {:?}: {}", path, e));
    let hash = Sha256::digest(&bytes);
    hex::encode(hash)
}
