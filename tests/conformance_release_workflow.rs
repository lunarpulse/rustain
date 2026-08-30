//! Release-workflow ratchets that stay inside the standalone `rustain` checkout.
//!
//! Story 19.5 (A11): nothing read `release.yml`, so the next person to touch the
//! `--features` line could delete the provide column with every lane green. These
//! assertions are the ratchet. ⛔ They read no path outside `rustain/` — `_bmad-output/`
//! is a different repository and is absent from the CI checkout (A8).

const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const RELEASE_BUILD: &str = "run: cargo build --release --target ${{ matrix.target }} --features self-update,relay-server,a2a,p2p";
const NFR9_BOUND_BYTES: &str = "36700160";
const SIZE_GUARD: &str = "if [ \"$SIZE\" -gt \"36700160\" ]; then";
const SMOKE_STEP: &str = "      - name: Smoke required release features\n        if: matrix.target == 'x86_64-unknown-linux-gnu'\n        shell: bash\n        run: .github/scripts/release-smoke.sh \"${ASSET}\"\n";
const SMOKE_SCRIPT: &str = ".github/scripts/release-smoke.sh";

#[test]
fn release_build_ships_a2a_and_p2p_with_the_measured_size_bound() {
    assert!(
        WORKFLOW.lines().any(|line| line.trim() == RELEASE_BUILD),
        "release build must name self-update, relay-server, a2a, and p2p explicitly"
    );
    assert_eq!(
        WORKFLOW.matches(NFR9_BOUND_BYTES).count(),
        1,
        "the NFR9 byte bound must appear exactly once in release.yml"
    );
    // Counting the constant is not enough on its own: it would still pass with the
    // number parked in a comment and the comparison changed. Pin the comparison.
    assert!(
        WORKFLOW.lines().any(|line| line.trim() == SIZE_GUARD),
        "the size guard must compare against the NFR9 byte bound itself"
    );
}

#[test]
fn the_linux_leg_smokes_the_release_asset_through_an_in_repo_script() {
    assert!(
        WORKFLOW.contains(SMOKE_STEP),
        "the Linux leg must invoke {SMOKE_SCRIPT} on the staged asset (AC2)"
    );

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SMOKE_SCRIPT);
    let metadata = std::fs::metadata(&script)
        .unwrap_or_else(|e| panic!("{} must exist in the checkout: {e}", script.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            metadata.permissions().mode() & 0o111 != 0,
            "{} must be executable — the release step runs it directly",
            script.display()
        );
    }
    #[cfg(not(unix))]
    let _ = metadata;
}

/// The `sign` job discloses *expected minus present* targets in the release body
/// (AC6). An expected target no matrix leg builds makes that disclosure fire on
/// every release, which is how a real missing macOS asset gets lost in the noise.
#[test]
fn the_expected_target_set_is_exactly_the_build_matrix() {
    let matrix: Vec<&str> = WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .collect();
    assert_eq!(matrix.len(), 3, "matrix targets changed: {matrix:?}");

    let mut expected: Vec<&str> = WORKFLOW
        .lines()
        .skip_while(|line| line.trim() != "expected_targets=(")
        .skip(1)
        .take_while(|line| line.trim() != ")")
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        !expected.is_empty(),
        "the sign job must enumerate the expected release targets"
    );

    let mut matrix = matrix;
    matrix.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        expected, matrix,
        "the sign job's expected targets must be exactly the targets the matrix builds"
    );
}
