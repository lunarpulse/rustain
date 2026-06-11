#![cfg(feature = "meta-search")]

//! Phase B conformance — `SearchHit.name` MUST round-trip through
//! `CapabilityId` for the relevant kind. Broken hits are caught at the
//! projection boundary, never reach the LLM (per ADR-09-02 v2 §LLM-Only Payload).

use rustain::domain::models::{
    capability_id::CapabilityId, capability_kind::CapabilityKind, search_hit::SearchHit,
};

#[test]
fn test_mcp_tool_name_round_trips_via_from_mcp_wire_name() {
    let hit = SearchHit::minimal(
        "mcp__postgres__query",
        CapabilityKind::Tool,
        "Run SQL.",
        7.0,
    );
    let id = CapabilityId::from_mcp_wire_name(&hit.name);
    assert!(
        id.is_some(),
        "MCP tool SearchHit.name '{}' must round-trip via CapabilityId::from_mcp_wire_name",
        hit.name
    );
    let id = id.unwrap();
    assert_eq!(id.protocol, "mcp");
    assert_eq!(id.server, "postgres");
    assert_eq!(id.tool, "query");
}

#[test]
fn test_builtin_tool_name_round_trips_via_capability_id_parse() {
    let hit = SearchHit::minimal("Bash", CapabilityKind::Tool, "Execute shell commands.", 8.4);
    // Builtin tools use the bare name on the wire. The registry-side form is
    // `builtin::<name>` via `CapabilityId::parse`.
    let registry_form = format!("builtin::{}", hit.name);
    let id = CapabilityId::parse(&registry_form);
    assert!(
        id.is_some(),
        "builtin tool name '{}' must round-trip via CapabilityId::parse",
        registry_form
    );
}

#[test]
fn test_skill_name_matches_anthropic_frontmatter_regex() {
    let hit = SearchHit::minimal(
        "review-code",
        CapabilityKind::Skill,
        "Reviews code when the user requests a review.",
        6.5,
    );
    let regex = regex::Regex::new(r"^[a-z][a-z0-9-]{0,63}$").unwrap();
    assert!(
        regex.is_match(&hit.name),
        "skill SearchHit.name '{}' MUST match the Anthropic Skills frontmatter contract regex \
         (lowercase, hyphen, ≤64 chars, leading letter)",
        hit.name
    );
}

#[test]
fn test_broken_hit_caught_at_projection_boundary() {
    // A `SearchHit` constructed with a malformed MCP name MUST be detectable
    // by the round-trip check at the projection boundary. The `IndexableItem::to_search_hit`
    // impl on `ToolDescriptor` is responsible for ensuring the produced hit
    // round-trips; this test is the canary.
    let broken = SearchHit::minimal("garbage::not::mcp::wire", CapabilityKind::Tool, "x", 0.0);
    assert!(
        CapabilityId::from_mcp_wire_name(&broken.name).is_none(),
        "Malformed MCP wire name must NOT round-trip — broken hits should be filtered \
         at the projection boundary, never reach the LLM"
    );
}
