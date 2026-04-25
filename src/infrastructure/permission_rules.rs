//! Permission rule loader and evaluator.
//!
//! See ADR-06-07 for first-match-wins design.

use globset::Glob;

/// Parsed action for a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleAction {
    Allow,
    Deny,
    Ask,
}

/// Scope that a rule applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleScope {
    Tool,
    Server,
    Path,
}

/// A single permission rule.
#[derive(Clone, Debug)]
pub struct Rule {
    pub priority: Option<i32>,
    pub pattern: String,
    pub action: RuleAction,
    pub scope: RuleScope,
}

/// Compiled rule set with compiled glob matchers.
pub struct RuleSet {
    pub rules: Vec<Rule>,
    /// Compiled glob matchers in the same order as `rules`.
    pub matchers: Vec<Glob>,
}

impl RuleSet {
    /// Seed a `SessionApprovalSet` from the allow-rules in this set.
    pub fn seed_session(&self) -> crate::domain::services::approval_runtime::SessionApprovalSet {
        let mut set = crate::domain::services::approval_runtime::SessionApprovalSet::default();
        for rule in &self.rules {
            if matches!(rule.action, RuleAction::Allow) {
                match rule.scope {
                    RuleScope::Tool => {
                        let tool_name = pattern_to_tool_name(&rule.pattern);
                        set.always_tools.insert(tool_name);
                    }
                    RuleScope::Server => {
                        set.always_servers.insert(rule.pattern.clone());
                    }
                    RuleScope::Path => {
                        set.always_paths.push(rule.pattern.clone());
                    }
                }
            }
        }
        set
    }

    /// Return true if a catch-all `*` / `ask` / `tool` rule exists.
    pub fn has_catchall(&self) -> bool {
        self.rules.iter().any(|r| {
            r.pattern == "*" && matches!(r.action, RuleAction::Ask) && matches!(r.scope, RuleScope::Tool)
        })
    }
}

/// Convert a pattern like `Bash:*` or `Bash` into a tool name.
fn pattern_to_tool_name(pattern: &str) -> String {
    if let Some(colon) = pattern.find(':') {
        pattern[..colon].to_string()
    } else {
        pattern.to_string()
    }
}

/// Load rules from both user config and workspace rules files.
pub fn load_rules(
    user_config_path: &std::path::Path,
    workspace_rules_path: &std::path::Path,
) -> Result<RuleSet, PermissionRulesError> {
    let mut rules: Vec<Rule> = Vec::new();

    // 1. User config: [permissions] always_tools / always_servers
    if let Ok(content) = std::fs::read_to_string(user_config_path) {
        if let Ok(table) = content.parse::<toml::Table>() {
            if let Some(permissions) = table.get("permissions").and_then(|v| v.as_table()) {
                if let Some(tools) = permissions.get("always_tools").and_then(|v| v.as_array()) {
                    for t in tools {
                        if let Some(s) = t.as_str() {
                            rules.push(Rule {
                                priority: Some(50),
                                pattern: format!("{}:*", s),
                                action: RuleAction::Allow,
                                scope: RuleScope::Tool,
                            });
                        }
                    }
                }
                if let Some(servers) = permissions.get("always_servers").and_then(|v| v.as_array()) {
                    for s in servers {
                        if let Some(s) = s.as_str() {
                            rules.push(Rule {
                                priority: Some(50),
                                pattern: s.to_string(),
                                action: RuleAction::Allow,
                                scope: RuleScope::Server,
                            });
                        }
                    }
                }
            }
        }
    }

    // 2. Workspace rules: [[rules]] array
    if let Ok(content) = std::fs::read_to_string(workspace_rules_path) {
        if let Ok(table) = content.parse::<toml::Table>() {
            if let Some(rules_arr) = table.get("rules").and_then(|v| v.as_array()) {
                for r in rules_arr {
                    if let Some(rule_table) = r.as_table() {
                        let priority = rule_table.get("priority").and_then(|v| v.as_integer()).map(|v| v as i32);
                        let pattern = rule_table.get("pattern").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let action = match rule_table.get("action").and_then(|v| v.as_str()) {
                            Some("allow") => RuleAction::Allow,
                            Some("deny") => RuleAction::Deny,
                            Some("ask") | _ => RuleAction::Ask,
                        };
                        let scope = match rule_table.get("scope").and_then(|v| v.as_str()) {
                            Some("server") => RuleScope::Server,
                            Some("path") => RuleScope::Path,
                            Some("tool") | _ => RuleScope::Tool,
                        };
                        rules.push(Rule { priority, pattern, action, scope });
                    }
                }
            }
        }
    }

    // Sort descending by priority (None → 0), stable sort preserves file order for ties.
    rules.sort_by(|a, b| b.priority.unwrap_or(0).cmp(&a.priority.unwrap_or(0)));

    // Compile globs; skip rules with invalid patterns (graceful degradation).
    let mut compiled_rules = Vec::with_capacity(rules.len());
    let mut compiled_matchers = Vec::with_capacity(rules.len());
    for rule in rules {
        match Glob::new(&rule.pattern) {
            Ok(glob) => {
                compiled_matchers.push(glob);
                compiled_rules.push(rule);
            }
            Err(e) => {
                tracing::warn!("Invalid glob pattern '{}' skipped: {}", rule.pattern, e);
            }
        }
    }

    Ok(RuleSet { rules: compiled_rules, matchers: compiled_matchers })
}

/// Error type for permission rule loading.
#[derive(Debug)]
pub enum PermissionRulesError {
    Io(()),
}

impl From<std::io::Error> for PermissionRulesError {
    fn from(e: std::io::Error) -> Self {
        PermissionRulesError::Io(())
    }
}
