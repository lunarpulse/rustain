// Covers: UX-DR74 (input history), AC3, AC7
//! Unit tests for InputHistory buffer: push, bounded eviction, up/down navigation, draft preservation, search.

use rustain::adapters::tui::state::InputHistory;

// --- Push and Bounded Eviction ---

#[test]
fn test_push_adds_entry() {
    let mut hist = InputHistory::new();
    hist.push("hello".to_string());
    assert_eq!(hist.len(), 1);
}

#[test]
fn test_push_empty_string_ignored() {
    let mut hist = InputHistory::new();
    hist.push("".to_string());
    assert_eq!(hist.len(), 0);
}

#[test]
fn test_push_multiple_entries() {
    let mut hist = InputHistory::new();
    hist.push("first".to_string());
    hist.push("second".to_string());
    hist.push("third".to_string());
    assert_eq!(hist.len(), 3);
}

#[test]
fn test_push_evicts_oldest_at_capacity() {
    let mut hist = InputHistory::new();
    for i in 0..100 {
        hist.push(format!("entry-{}", i));
    }
    assert_eq!(hist.len(), 100);

    // Push one more — oldest should be evicted
    hist.push("entry-100".to_string());
    assert_eq!(hist.len(), 100);

    // Navigate to oldest — should be entry-1, not entry-0
    let oldest = hist.navigate_up("");
    assert!(oldest.is_some());
    // Navigate all the way up
    for _ in 0..99 {
        hist.navigate_up("");
    }
    // At the oldest entry, which should be "entry-1"
    let result = hist.navigate_up("");
    assert_eq!(result, Some("entry-1"));
}

// --- Navigation Up/Down ---

#[test]
fn test_navigate_up_empty_history() {
    let mut hist = InputHistory::new();
    assert_eq!(hist.navigate_up(""), None);
}

#[test]
fn test_navigate_up_returns_newest_first() {
    let mut hist = InputHistory::new();
    hist.push("first".to_string());
    hist.push("second".to_string());
    hist.push("third".to_string());

    assert_eq!(hist.navigate_up(""), Some("third"));
    assert_eq!(hist.navigate_up(""), Some("second"));
    assert_eq!(hist.navigate_up(""), Some("first"));
}

#[test]
fn test_navigate_up_stops_at_oldest() {
    let mut hist = InputHistory::new();
    hist.push("only".to_string());

    assert_eq!(hist.navigate_up(""), Some("only"));
    assert_eq!(hist.navigate_up(""), Some("only")); // stays at oldest
}

#[test]
fn test_navigate_down_from_middle() {
    let mut hist = InputHistory::new();
    hist.push("first".to_string());
    hist.push("second".to_string());
    hist.push("third".to_string());

    // Go up twice
    hist.navigate_up("");
    hist.navigate_up("");
    // Now at "second"

    // Go down — should return "third"
    assert_eq!(hist.navigate_down(), Some("third"));
}

#[test]
fn test_navigate_down_past_newest_returns_draft() {
    let mut hist = InputHistory::new();
    hist.push("first".to_string());
    hist.push("second".to_string());

    // Start navigating with a draft
    hist.navigate_up("my draft");
    // At "second"

    // Go down past newest → returns draft
    assert_eq!(hist.navigate_down(), Some("my draft"));
}

#[test]
fn test_navigate_down_when_not_navigating() {
    let mut hist = InputHistory::new();
    hist.push("first".to_string());
    assert_eq!(hist.navigate_down(), None);
}

// --- Draft Preservation ---

#[test]
fn test_draft_preserved_on_navigate_up() {
    let mut hist = InputHistory::new();
    hist.push("old entry".to_string());

    hist.navigate_up("current typing");
    assert_eq!(hist.draft(), "current typing");
}

#[test]
fn test_draft_restored_on_navigate_down_past_end() {
    let mut hist = InputHistory::new();
    hist.push("entry1".to_string());

    hist.navigate_up("draft text");
    let result = hist.navigate_down();
    assert_eq!(result, Some("draft text"));
}

#[test]
fn test_reset_navigation_clears_draft() {
    let mut hist = InputHistory::new();
    hist.push("entry".to_string());

    hist.navigate_up("some draft");
    assert!(hist.is_navigating());

    hist.reset_navigation();
    assert!(!hist.is_navigating());
    assert_eq!(hist.draft(), "");
}

// --- Search ---

#[test]
fn test_search_empty_query() {
    let mut hist = InputHistory::new();
    hist.push("hello".to_string());
    let results = hist.search("");
    assert!(results.is_empty());
}

#[test]
fn test_search_no_match() {
    let mut hist = InputHistory::new();
    hist.push("hello world".to_string());
    let results = hist.search("xyz");
    assert!(results.is_empty());
}

#[test]
fn test_search_case_insensitive() {
    let mut hist = InputHistory::new();
    hist.push("Hello World".to_string());
    hist.push("goodbye world".to_string());

    let results = hist.search("hello");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Hello World");
}

#[test]
fn test_search_substring_match() {
    let mut hist = InputHistory::new();
    hist.push("add authentication middleware".to_string());
    hist.push("check auth token expiry".to_string());
    hist.push("debug auth flow".to_string());
    hist.push("unrelated command".to_string());

    let results = hist.search("auth");
    assert_eq!(results.len(), 3);
    // Results should be in reverse order (newest first)
    assert_eq!(results[0].1, "debug auth flow");
    assert_eq!(results[1].1, "check auth token expiry");
    assert_eq!(results[2].1, "add authentication middleware");
}

#[test]
fn test_search_returns_indices() {
    let mut hist = InputHistory::new();
    hist.push("alpha".to_string());
    hist.push("beta".to_string());
    hist.push("alpha2".to_string());

    let results = hist.search("alpha");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, 2); // alpha2 is at index 2
    assert_eq!(results[1].0, 0); // alpha is at index 0
}

// --- Push resets navigation ---

#[test]
fn test_push_resets_navigation() {
    let mut hist = InputHistory::new();
    hist.push("first".to_string());
    hist.push("second".to_string());

    hist.navigate_up("draft");
    assert!(hist.is_navigating());

    hist.push("third".to_string());
    assert!(!hist.is_navigating());
}

// --- Full navigation cycle ---

#[test]
fn test_full_navigation_cycle() {
    let mut hist = InputHistory::new();
    hist.push("msg1".to_string());
    hist.push("msg2".to_string());
    hist.push("msg3".to_string());

    // Start with a draft
    assert_eq!(hist.navigate_up("typing"), Some("msg3"));
    assert_eq!(hist.navigate_up("typing"), Some("msg2"));
    assert_eq!(hist.navigate_up("typing"), Some("msg1"));
    assert_eq!(hist.navigate_up("typing"), Some("msg1")); // stuck at top

    // Go back down
    assert_eq!(hist.navigate_down(), Some("msg2"));
    assert_eq!(hist.navigate_down(), Some("msg3"));
    assert_eq!(hist.navigate_down(), Some("typing")); // back to draft
    assert_eq!(hist.navigate_down(), None); // no longer navigating
}

// --- 100-entry bound ---

#[test]
fn test_100_entry_bound() {
    let mut hist = InputHistory::new();
    for i in 0..150 {
        hist.push(format!("msg{}", i));
    }
    assert_eq!(hist.len(), 100);
}
