//! Profile loader — pure domain service for loading, extending, and validating
//! profile definitions. Takes raw TOML strings through the `ProfileSource` trait
//! so it never touches the filesystem directly.
//!
//! Story 8.2 AC-1, AC-2, AC-8.

use crate::domain::errors::ProfileError;
use crate::domain::models::{
    AdapterRef, PortDimension, ProfileDefinition, ProfileSelection, ResolvedProfile,
};
use crate::domain::services::adapter_catalog::AdapterCatalog;
use std::collections::BTreeMap;

/// Source of raw profile TOML strings by name.
/// Implementations: `EmbeddedProfileSource` (built-in), `FileSystemProfileSource` (user ~/.config).
pub trait ProfileSource {
    fn get(&self, name: &str) -> Option<String>;
}

/// Stateless profile loader. Consumes a `&AdapterCatalog` and a `&dyn ProfileSource`.
pub struct ProfileLoader<'a> {
    catalog: &'a AdapterCatalog,
    source: &'a dyn ProfileSource,
}

impl<'a> ProfileLoader<'a> {
    pub fn new(catalog: &'a AdapterCatalog, source: &'a dyn ProfileSource) -> Self {
        Self { catalog, source }
    }

    /// Public entry point — load + extends-resolve + validate + build selection.
    pub fn load(&self, name: &str) -> Result<ResolvedProfile, ProfileError> {
        let mut visited: Vec<String> = vec![name.to_string()];
        let merged = self.resolve_extends(name, &mut visited, 1)?;
        let validated = self.validate(&merged, name)?;
        let selection = self.build_selection(&validated);
        let overrides = validated.overrides.and_then(|tv| toml_value_to_figment(tv));

        Ok(ResolvedProfile {
            name: name.to_string(),
            selection,
            overrides,
            preview: validated.preview,
            mcp_servers: Vec::new(),
            a2a_peers: Vec::new(),
            include_builtin_tools: true,
        })
    }

    /// Parse a raw TOML string into a `ProfileDefinition`.
    fn parse(
        toml_str: &str,
        source_path: std::path::PathBuf,
    ) -> Result<ProfileDefinition, ProfileError> {
        toml::from_str::<ProfileDefinition>(toml_str).map_err(|e| ProfileError::Parse {
            path: source_path,
            reason: e.to_string(),
        })
    }

    /// Recursively resolve an extends chain with cycle detection and depth limit.
    fn resolve_extends(
        &self,
        name: &str,
        visited: &mut Vec<String>,
        depth: u32,
    ) -> Result<ProfileDefinition, ProfileError> {
        if depth > 4 {
            return Err(ProfileError::ExtendsTooDeep {
                chain: visited.clone(),
            });
        }

        let toml_str = self.source.get(name).ok_or_else(|| {
            let err = if depth == 1 {
                ProfileError::ProfileNotFound {
                    name: name.to_string(),
                    search_paths: vec![],
                }
            } else {
                ProfileError::ParentNotFound {
                    child: visited
                        .get(visited.len().wrapping_sub(2))
                        .cloned()
                        .unwrap_or_default(),
                    parent: name.to_string(),
                    search_paths: vec![],
                }
            };
            tracing::error!("{}", err);
            err
        })?;

        let source_path = std::path::PathBuf::from(format!("profile:{name}"));
        let mut def = Self::parse(&toml_str, source_path)?;

        // Resolve parent
        if let Some(ref parent) = def.extends.clone() {
            // Cycle detection
            if visited.contains(parent) {
                visited.push(parent.clone());
                let err = ProfileError::CircularExtends {
                    chain: visited.clone(),
                };
                tracing::error!("{}", err);
                return Err(err);
            }
            visited.push(parent.clone());
            let parent_def = self.resolve_extends(parent, visited, depth + 1)?;
            // Merge: child overrides parent per-dimension
            def = Self::merge(parent_def, def);
        }

        Ok(def)
    }

    /// Merge a parent and child ProfileDefinition. Child's per-dimension entries
    /// completely override the parent's (whole-replacement, NOT per-field). Child's
    /// `overrides` toml::Value is deep-merged with parent's.
    fn merge(parent: ProfileDefinition, child: ProfileDefinition) -> ProfileDefinition {
        let overrides = merge_toml_values(parent.overrides, child.overrides);

        ProfileDefinition {
            name: child.name,
            description: child.description.or(parent.description),
            extends: None,
            identity_color: child.identity_color.or(parent.identity_color),
            preview: child.preview || parent.preview,
            persona: child.persona.or(parent.persona),
            memory: child.memory.or(parent.memory),
            session: child.session.or(parent.session),
            tools: child.tools.or(parent.tools),
            channels: child.channels.or(parent.channels),
            scheduler: child.scheduler.or(parent.scheduler),
            context: child.context.or(parent.context),
            overrides,
        }
    }

    /// Validate the merged profile definition against the AdapterCatalog.
    fn validate(
        &self,
        def: &ProfileDefinition,
        profile_name: &str,
    ) -> Result<ProfileDefinition, ProfileError> {
        // Check all 7 dimensions present
        let mut missing = Vec::new();
        if def.persona.is_none() {
            missing.push(PortDimension::Persona);
        }
        if def.memory.is_none() {
            missing.push(PortDimension::Memory);
        }
        if def.session.is_none() {
            missing.push(PortDimension::Session);
        }
        if def.tools.is_none() {
            missing.push(PortDimension::Tools);
        }
        if def.channels.is_none() {
            missing.push(PortDimension::Channels);
        }
        if def.scheduler.is_none() {
            missing.push(PortDimension::Scheduler);
        }
        if def.context.is_none() {
            missing.push(PortDimension::Context);
        }

        if !missing.is_empty() {
            return Err(ProfileError::DimensionMissing {
                profile: profile_name.to_string(),
                dimensions: missing,
            });
        }

        // Validate each adapter reference against the catalog
        self.validate_adapter(
            def,
            PortDimension::Persona,
            def.persona.as_ref().unwrap(),
            profile_name,
        )?;
        self.validate_adapter(
            def,
            PortDimension::Memory,
            def.memory.as_ref().unwrap(),
            profile_name,
        )?;
        self.validate_adapter(
            def,
            PortDimension::Session,
            def.session.as_ref().unwrap(),
            profile_name,
        )?;
        self.validate_adapter(
            def,
            PortDimension::Tools,
            def.tools.as_ref().unwrap(),
            profile_name,
        )?;
        self.validate_adapter(
            def,
            PortDimension::Channels,
            def.channels.as_ref().unwrap(),
            profile_name,
        )?;
        self.validate_adapter(
            def,
            PortDimension::Scheduler,
            def.scheduler.as_ref().unwrap(),
            profile_name,
        )?;
        self.validate_adapter(
            def,
            PortDimension::Context,
            def.context.as_ref().unwrap(),
            profile_name,
        )?;

        Ok(def.clone())
    }

    fn validate_adapter(
        &self,
        def: &ProfileDefinition,
        port: PortDimension,
        adapter_ref: &AdapterRef,
        profile_name: &str,
    ) -> Result<(), ProfileError> {
        let name = &adapter_ref.adapter;

        // Check adapter is known
        let desc = AdapterCatalog::lookup(port, name);
        if desc.is_none() {
            let available = AdapterCatalog::known_for(port);
            let suggestion = closest_match(name, &available, 1);
            return Err(ProfileError::AdapterUnknown {
                profile: profile_name.to_string(),
                port,
                adapter: name.clone(),
                available: available.iter().map(|s| s.to_string()).collect(),
                suggestion,
            });
        }

        let desc = desc.unwrap();
        // Check feature gate: fatal for non-preview, warning for preview
        if let Some(feature) = desc.feature_gate {
            if !AdapterCatalog::is_feature_compiled(feature) && !def.preview {
                return Err(ProfileError::AdapterFeatureGated {
                    profile: profile_name.to_string(),
                    port,
                    adapter: name.clone(),
                    feature: feature.to_string(),
                });
            }
        }

        Ok(())
    }

    /// Build a ProfileSelection from a validated ProfileDefinition.
    /// For preview profiles, applies fallback rewriting for feature-gated adapters.
    fn build_selection(&self, def: &ProfileDefinition) -> ProfileSelection {
        let mut dimensions = BTreeMap::new();

        let adapters: [(PortDimension, &Option<AdapterRef>); 7] = [
            (PortDimension::Persona, &def.persona),
            (PortDimension::Memory, &def.memory),
            (PortDimension::Session, &def.session),
            (PortDimension::Tools, &def.tools),
            (PortDimension::Channels, &def.channels),
            (PortDimension::Scheduler, &def.scheduler),
            (PortDimension::Context, &def.context),
        ];

        for (port, adapter_opt) in &adapters {
            if let Some(adapter) = adapter_opt {
                let mut final_adapter = adapter.clone();
                // Preview profiles advertise feature-gated adapters (e.g. telegram,
                // cron) but must always compose safely, so rewrite them to their
                // fallback regardless of whether the cargo feature is enabled.
                if def.preview {
                    if let Some(desc) = AdapterCatalog::lookup(*port, &adapter.adapter) {
                        if desc.feature_gate.is_some() {
                            if let Some(fallback) = desc.fallback {
                                final_adapter.adapter = fallback.to_string();
                            }
                        }
                    }
                }
                dimensions.insert(*port, final_adapter);
            }
        }

        ProfileSelection { dimensions }
    }
}

/// Convert a `toml::Value` to a `figment::value::Value` via serde round-trip.
fn toml_value_to_figment(tv: toml::Value) -> Option<figment::value::Value> {
    figment::value::Value::serialize(&tv).ok()
}
fn merge_toml_values(
    parent: Option<toml::Value>,
    child: Option<toml::Value>,
) -> Option<toml::Value> {
    match (parent, child.clone()) {
        (None, None) => None,
        (Some(p), None) => Some(p),
        (None, Some(c)) => Some(c),
        (Some(toml::Value::Table(mut p)), Some(toml::Value::Table(c))) => {
            for (k, v) in c {
                match p.get_mut(&k) {
                    Some(existing) if existing.is_table() && v.is_table() => {
                        let parent_sub = std::mem::replace(existing, toml::Value::Boolean(false));
                        let merged_table = merge_toml_values(Some(parent_sub), Some(v));
                        if let Some(merged) = merged_table {
                            *existing = merged;
                        }
                    }
                    _ => {
                        p.insert(k, v);
                    }
                }
            }
            Some(toml::Value::Table(p))
        }
        _ => child.clone(), // scalar override
    }
}

/// Find the closest string in `candidates` within Levenshtein distance `max_dist`.
pub fn closest_match(target: &str, candidates: &[&str], max_dist: usize) -> Option<String> {
    candidates
        .iter()
        .filter_map(|c| {
            let dist = levenshtein_distance(target, c);
            if dist <= max_dist {
                Some((dist, c.to_string()))
            } else {
                None
            }
        })
        .min_by_key(|(dist, _)| *dist)
        .map(|(_, s)| s)
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let alen = a_chars.len();
    let blen = b_chars.len();

    if alen == 0 {
        return blen;
    }
    if blen == 0 {
        return alen;
    }

    let mut prev: Vec<usize> = (0..=blen).collect();
    let mut curr = vec![0usize; blen + 1];

    for i in 1..=alen {
        curr[0] = i;
        for j in 1..=blen {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[blen]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory profile source for testing.
    struct TestProfileSource {
        profiles: RefCell<HashMap<String, String>>,
    }

    impl TestProfileSource {
        fn new(entries: Vec<(&str, &str)>) -> Self {
            let mut m = HashMap::new();
            for (k, v) in entries {
                m.insert(k.to_string(), v.to_string());
            }
            Self {
                profiles: RefCell::new(m),
            }
        }
    }

    impl ProfileSource for TestProfileSource {
        fn get(&self, name: &str) -> Option<String> {
            self.profiles.borrow().get(name).cloned()
        }
    }

    fn make_loader<'a>(source: &'a dyn ProfileSource) -> ProfileLoader<'a> {
        ProfileLoader::new(&AdapterCatalog, source)
    }

    fn base_toml() -> String {
        r#"name = "base"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#
        .to_string()
    }

    #[test]
    fn simple_profile_no_extends() {
        let source = TestProfileSource::new(vec![("base", &base_toml())]);
        let loader = make_loader(&source);
        let resolved = loader.load("base").unwrap();
        assert_eq!(resolved.name, "base");
        assert_eq!(resolved.selection.dimensions.len(), 7);
        assert_eq!(
            resolved.selection.dimensions[&PortDimension::Persona].adapter,
            "minimal"
        );
        assert_eq!(
            resolved.selection.dimensions[&PortDimension::Memory].adapter,
            "noop"
        );
    }

    #[test]
    fn single_extends_overrides_dimensions() {
        let coding_toml = r#"name = "coding"
extends = "base"
[persona]
adapter = "coding"
[memory]
adapter = "project-scoped"
[session]
adapter = "workspace"
[tools]
adapter = "builtin-full"
"#;
        let source = TestProfileSource::new(vec![("base", &base_toml()), ("coding", coding_toml)]);
        let loader = make_loader(&source);
        let resolved = loader.load("coding").unwrap();
        // Child overrides
        assert_eq!(
            resolved.selection.dimensions[&PortDimension::Persona].adapter,
            "coding"
        );
        // Inherited from parent
        assert_eq!(
            resolved.selection.dimensions[&PortDimension::Channels].adapter,
            "terminal"
        );
    }

    #[test]
    fn four_deep_extends_chain_succeeds() {
        let level1 = r#"name = "l1"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;
        let level2 = r#"name = "l2"
extends = "l1"
"#;
        let level3 = r#"name = "l3"
extends = "l2"
"#;
        let level4 = r#"name = "l4"
extends = "l3"
"#;
        let source = TestProfileSource::new(vec![
            ("l1", level1),
            ("l2", level2),
            ("l3", level3),
            ("l4", level4),
        ]);
        let loader = make_loader(&source);
        let resolved = loader.load("l4").unwrap();
        assert_eq!(resolved.selection.dimensions.len(), 7);
    }

    #[test]
    fn five_deep_extends_chain_fails() {
        let base = base_toml();
        let l2 = "name = \"l2\"\nextends = \"l1\"\n";
        let l3 = "name = \"l3\"\nextends = \"l2\"\n";
        let l4 = "name = \"l4\"\nextends = \"l3\"\n";
        let l5 = "name = \"l5\"\nextends = \"l4\"\n";
        let source = TestProfileSource::new(vec![
            ("l1", &base),
            ("l2", l2),
            ("l3", l3),
            ("l4", l4),
            ("l5", l5),
        ]);
        let loader = make_loader(&source);
        let err = loader.load("l5").unwrap_err();
        assert!(matches!(err, ProfileError::ExtendsTooDeep { .. }));
    }

    #[test]
    fn self_cycle_detected() {
        let toml = "name = \"cycle\"\nextends = \"cycle\"\n[persona]\nadapter = \"minimal\"\n[memory]\nadapter = \"noop\"\n[session]\nadapter = \"basic\"\n[tools]\nadapter = \"builtin-only\"\n[channels]\nadapter = \"terminal\"\n[scheduler]\nadapter = \"none\"\n[context]\nadapter = \"default\"\n";
        let source = TestProfileSource::new(vec![("cycle", toml)]);
        let loader = make_loader(&source);
        let err = loader.load("cycle").unwrap_err();
        assert!(matches!(err, ProfileError::CircularExtends { .. }));
    }

    #[test]
    fn three_cycle_detected() {
        let a_toml = "name = \"a\"\nextends = \"b\"\n";
        let b_toml = "name = \"b\"\nextends = \"c\"\n";
        let c_toml = "name = \"c\"\nextends = \"a\"\n";
        let source = TestProfileSource::new(vec![("a", a_toml), ("b", b_toml), ("c", c_toml)]);
        let loader = make_loader(&source);
        let err = loader.load("a").unwrap_err();
        assert!(matches!(err, ProfileError::CircularExtends { .. }));
    }

    #[test]
    fn missing_parent_error() {
        let toml = r#"name = "orphan"
extends = "nonexistent"
"#;
        let source = TestProfileSource::new(vec![("orphan", toml)]);
        let loader = make_loader(&source);
        let err = loader.load("orphan").unwrap_err();
        assert!(matches!(err, ProfileError::ParentNotFound { .. }));
    }

    #[test]
    fn unknown_adapter_returns_error_with_suggestion() {
        let toml = r#"name = "bad"
[persona]
adapter = "minimal"
[memory]
adapter = "projct-scoped"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;
        let source = TestProfileSource::new(vec![("bad", toml)]);
        let loader = make_loader(&source);
        let err = loader.load("bad").unwrap_err();
        match err {
            ProfileError::AdapterUnknown { suggestion, .. } => {
                assert_eq!(suggestion.as_deref(), Some("project-scoped"));
            }
            _ => panic!("expected AdapterUnknown"),
        }
    }

    #[test]
    fn dimension_missing_lists_all_missing() {
        let toml = r#"name = "incomplete"
[persona]
adapter = "minimal"
"#;
        let source = TestProfileSource::new(vec![("incomplete", toml)]);
        let loader = make_loader(&source);
        let err = loader.load("incomplete").unwrap_err();
        match err {
            ProfileError::DimensionMissing { dimensions, .. } => {
                assert_eq!(dimensions.len(), 6);
            }
            _ => panic!("expected DimensionMissing"),
        }
    }

    #[test]
    fn preview_profile_rewrites_feature_gated_adapters() {
        let toml = r#"name = "test-preview"
preview = true
[persona]
adapter = "personal-assistant"
[memory]
adapter = "daily-log"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "telegram"
[scheduler]
adapter = "cron"
[context]
adapter = "daily"
"#;
        let source = TestProfileSource::new(vec![("test-preview", toml)]);
        let loader = make_loader(&source);
        let resolved = loader.load("test-preview").unwrap();
        if !cfg!(feature = "telegram") {
            assert_eq!(
                resolved.selection.dimensions[&PortDimension::Channels].adapter,
                "terminal"
            );
        }
        if !cfg!(feature = "cron") {
            assert_eq!(
                resolved.selection.dimensions[&PortDimension::Scheduler].adapter,
                "none"
            );
        }
    }

    #[test]
    fn non_preview_profile_feature_gated_adapter_is_fatal() {
        let toml = r#"name = "no-preview"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "telegram"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;
        let source = TestProfileSource::new(vec![("no-preview", toml)]);
        let loader = make_loader(&source);
        if !cfg!(feature = "telegram") {
            let err = loader.load("no-preview").unwrap_err();
            assert!(matches!(err, ProfileError::AdapterFeatureGated { .. }));
        }
    }

    #[test]
    fn overrides_merge_across_extends() {
        let parent_toml = r#"name = "parent"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
[overrides]
default_plan_mode = false
model = "parent-model"
"#;
        let child_toml = r#"name = "child"
extends = "parent"
[overrides]
model = "child-model"
log_level = "debug"
"#;
        let source = TestProfileSource::new(vec![("parent", parent_toml), ("child", child_toml)]);
        let loader = make_loader(&source);
        let resolved = loader.load("child").unwrap();
        let ov = resolved.overrides.unwrap();
        // parent's default_plan_mode is preserved, model is overridden, log_level is added
        assert!(format!("{:?}", ov).contains("default_plan_mode"));
        assert!(format!("{:?}", ov).contains("child-model"));
        assert!(format!("{:?}", ov).contains("debug"));
    }

    #[test]
    fn levenshtein_distance_correct() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("projct-scoped", "project-scoped"), 1);
    }
}
