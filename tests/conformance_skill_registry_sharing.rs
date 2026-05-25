//! Story 16-0 AC4: Post-rescan consistency test for shared SkillRegistry.
//!
//! Verifies that after a rescan, both the SkillActivator and TuiState see
//! the same catalog through the shared Arc<RwLock<SkillRegistry>>.

use std::sync::Arc;

use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::skill_registry::SkillRegistry;

/// AC4: Rescan → activator and TuiState (autocomplete path) see same skills.
#[tokio::test]
async fn test_skill_registry_sharing_after_rescan() {
    let shared = Arc::new(tokio::sync::RwLock::new(SkillRegistry::new()));
    let activator = SkillActivator::with_registry(Arc::clone(&shared));

    // Start with one skill (foo) — matches AC4 Gherkin "Given a workspace with one initial skill foo".
    let foo = rustain::domain::models::SkillDef {
        name: "foo".to_string(),
        description: "first skill".to_string(),
        file: std::path::PathBuf::from("/fake/foo/SKILL.md"),
        directory: std::path::PathBuf::from("/fake/foo"),
        source: rustain::domain::models::SkillSource::GlobalAgents,
        allowed_tools: None,
        terse: None,
    };
    {
        let mut guard = shared.write().await;
        *guard = SkillRegistry::from_skills(vec![foo.clone()]);
    }
    assert_eq!(
        activator.discovered_skill_names().await,
        vec!["foo".to_string()]
    );

    // Simulate a rescan that adds bar — matches AC4 Gherkin "When the test invokes a rescan that adds skill bar".
    let bar = rustain::domain::models::SkillDef {
        name: "bar".to_string(),
        description: "second skill".to_string(),
        file: std::path::PathBuf::from("/fake/bar/SKILL.md"),
        directory: std::path::PathBuf::from("/fake/bar"),
        source: rustain::domain::models::SkillSource::GlobalAgents,
        allowed_tools: None,
        terse: None,
    };
    let updated_registry = SkillRegistry::from_skills(vec![foo.clone(), bar.clone()]);
    {
        let mut guard = shared.write().await;
        *guard = updated_registry;
        // Guard drops here — no .await held across.
    }

    // AC4: Activator sees [foo, bar] (sorted).
    let names = activator.discovered_skill_names().await;
    assert_eq!(names, vec!["bar".to_string(), "foo".to_string()]);

    // AC4: The shared Arc also sees [foo, bar] — same catalog, no divergence.
    {
        let guard = shared.read().await;
        let filter_results: Vec<&str> = guard.filter("").iter().map(|s| s.name.as_str()).collect();
        assert_eq!(filter_results, vec!["bar", "foo"]);
    }
}

/// AC4: Contended path — write guard held briefly, then immediate read from another task.
#[tokio::test]
async fn test_contended_write_then_read() {
    let shared = Arc::new(tokio::sync::RwLock::new(SkillRegistry::new()));

    // Populate initial data.
    {
        let mut guard = shared.write().await;
        *guard = SkillRegistry::from_skills(vec![rustain::domain::models::SkillDef {
            name: "initial".to_string(),
            description: "test".to_string(),
            file: std::path::PathBuf::from("/fake/init/SKILL.md"),
            directory: std::path::PathBuf::from("/fake/init"),
            source: rustain::domain::models::SkillSource::GlobalAgents,
            allowed_tools: None,
            terse: None,
        }]);
    }

    let shared_clone = Arc::clone(&shared);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let barrier_clone = Arc::clone(&barrier);

    // Spawn a task that holds the write guard, then signals the main task to read.
    let handle = tokio::spawn(async move {
        // Write new data and drop guard before signaling.
        {
            let mut guard = shared_clone.write().await;
            *guard = SkillRegistry::from_skills(vec![rustain::domain::models::SkillDef {
                name: "updated".to_string(),
                description: "test".to_string(),
                file: std::path::PathBuf::from("/fake/upd/SKILL.md"),
                directory: std::path::PathBuf::from("/fake/upd"),
                source: rustain::domain::models::SkillSource::GlobalAgents,
                allowed_tools: None,
                terse: None,
            }]);
        }
        // Signal the main task that the write is complete.
        barrier_clone.wait().await;
        // Keep task alive briefly so the main task can race to read.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // Read should see the new data.
        let guard = shared_clone.read().await;
        guard
            .skills()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    });

    // Main task waits for the write to complete, then reads concurrently.
    barrier.wait().await;
    let main_names: Vec<String> = {
        let guard = shared.read().await;
        guard.skills().iter().map(|s| s.name.clone()).collect()
    };

    // Both tasks should see the updated data.
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("timed out")
        .expect("task panicked");

    assert_eq!(main_names, vec!["updated".to_string()]);
    assert_eq!(result, vec!["updated".to_string()]);
}

/// Verify that SkillRegistry::from_skills produces a discovery-ready registry.
#[test]
fn test_from_skills_reflects_skills() {
    let reg = SkillRegistry::from_skills(vec![rustain::domain::models::SkillDef {
        name: "test-skill".to_string(),
        description: "desc".to_string(),
        file: std::path::PathBuf::from("/fake/SKILL.md"),
        directory: std::path::PathBuf::from("/fake"),
        source: rustain::domain::models::SkillSource::GlobalAgents,
        allowed_tools: None,
        terse: None,
    }]);
    assert_eq!(reg.skills().len(), 1);
    assert!(reg.is_discovered());
    assert_eq!(reg.skills()[0].name, "test-skill");
}
