//! The K4-extension harness (Story 21.3 AC21): one `Case` per registry context.
//! A case drives every row's keys through the context's real local front door
//! from a fixture state, compares the outcome and a salient-state string BY VALUE
//! with the recorded witness, checks that every row of the context has a witness,
//! and sweeps every `Key::universe()` key that resolves to no row: it must do
//! nothing. Off-state witnesses pin the keys whose arm is state-guarded or typed
//! (R3); the exclusive contexts witness every row they inherit (R4).
#![allow(dead_code)]

use rustain::adapters::tui::keymap::{Context, Host, Key, Resolved, resolve, resolve::rows_of};
use rustain::adapters::tui::state::TuiState;

use super::support::{assert_global_effect, global_owned, is_inert, press};

/// One recorded witness: pressing `keys` from the case's fixture (after `prep`)
/// yields an outcome and a salient state whose `"{outcome} | {salient}"` text is
/// `expect`.
pub struct W {
    pub keys: &'static [Key],
    pub expect: &'static str,
    pub prep: Option<fn(&mut TuiState)>,
}

pub const fn w(keys: &'static [Key], expect: &'static str) -> W {
    W {
        keys,
        expect,
        prep: None,
    }
}

/// A witness whose fixture needs one more step (a sub-state: warning, preview).
pub const fn wp(keys: &'static [Key], expect: &'static str, prep: fn(&mut TuiState)) -> W {
    W {
        keys,
        expect,
        prep: Some(prep),
    }
}

pub struct Case {
    pub context: Context,
    /// The context's fixture: the state a user sees when the context is current.
    pub setup: fn() -> TuiState,
    /// The few fields that summarise the context for a before/after comparison.
    pub salient: fn(&TuiState) -> String,
    pub witnesses: &'static [W],
    /// Rowless keys that legitimately act (typed text, a documented fallthrough).
    pub acts_without_row: fn(Key) -> bool,
    /// The context's real front door: `local_door` (`handle_input`), or the
    /// host-neutral handler the event loop calls before `handle_input`
    /// (notification log, decision pane). Returns the outcome text and whether the
    /// outcome is inert (consumed or ignored: nothing was commanded).
    pub door: fn(&mut TuiState, Key) -> (String, bool),
    /// Off-state witnesses (R3): a key whose row is state-guarded (`y` with no
    /// warning) or that types text (`q j k G e /` into a search), pressed after
    /// `prep`. By value: inert, or the documented typed effect.
    pub off: &'static [W],
    /// Inherited-row witnesses (R4): the exclusive contexts (`ChatDrill`,
    /// `TasksReorder`) require one for every non-Global row they inherit.
    pub inherited: &'static [W],
}

/// A case driven through `app::handle_input`, with no off-state or inherited
/// witnesses and no rowless key that acts (struct-update the rest).
pub fn local(
    context: Context,
    setup: fn() -> TuiState,
    salient: fn(&TuiState) -> String,
    witnesses: &'static [W],
) -> Case {
    Case {
        context,
        setup,
        salient,
        witnesses,
        acts_without_row: never,
        door: local_door,
        off: &[],
        inherited: &[],
    }
}

/// `app::handle_input` through the real key decoder.
pub fn local_door(state: &mut TuiState, key: Key) -> (String, bool) {
    let action = press(state, key);
    (format!("{action:?}"), is_inert(&action))
}

pub fn never(_: Key) -> bool {
    false
}

/// Every printable ASCII key (a context whose query field types them).
pub fn printable(key: Key) -> bool {
    matches!(key, Key::Char(c) if (' '..='~').contains(&c))
}

fn play(case: &Case, w: &W) -> String {
    let mut state = (case.setup)();
    if let Some(prep) = w.prep {
        prep(&mut state);
    }
    let mut last = String::new();
    for key in w.keys {
        last = (case.door)(&mut state, *key).0;
    }
    format!("{last} | {}", (case.salient)(&state))
}

/// Run one case: witnesses by value, row coverage, negative sweep.
pub fn run(case: &Case) {
    let name = case.context.name();
    let rows: Vec<&[Key]> = rows_of(case.context, Host::Local).map(|r| r.keys).collect();
    for keys in &rows {
        assert!(
            case.witnesses.iter().any(|w| w.keys == *keys),
            "{name}: row {keys:?} has no witness"
        );
    }
    for w in case.witnesses {
        assert!(
            matches!(resolve(case.context, Host::Local, w.keys), Resolved::Hit(_)),
            "{name}: witness {:?} names no row of the context",
            w.keys
        );
    }
    let mut differ = Vec::new();
    for w in case.witnesses.iter().chain(case.off).chain(case.inherited) {
        let got = play(case, w);
        if got != w.expect {
            differ.push(format!("{:?}\n  want {}\n  got  {got}", w.keys, w.expect));
        }
    }
    assert!(
        differ.is_empty(),
        "{name}: witnesses differ:\n{}",
        differ.join("\n")
    );
    let (mut swept, mut inherited) = (0, Vec::new());
    for key in Key::universe() {
        if global_owned(case.context, key) {
            assert_global_effect(&mut (case.setup)(), key);
            continue;
        }
        match resolve(case.context, Host::Local, &[key]) {
            Resolved::Hit(row) if row.context != case.context => {
                inherited.push(key);
                continue;
            }
            Resolved::Miss if !(case.acts_without_row)(key) => {}
            _ => continue,
        }
        let mut state = (case.setup)();
        let before = (case.salient)(&state);
        let (outcome, inert) = (case.door)(&mut state, key);
        assert!(inert, "{name}: {key:?} returned {outcome}");
        assert_eq!(
            (case.salient)(&state),
            before,
            "{name}: {key:?} changed state"
        );
        swept += 1;
    }
    assert!(swept > 40, "{name}: the sweep ran ({swept} keys)");
    let witnessed = |key: Key| case.inherited.iter().any(|w| w.keys == [key]);
    let exclusive = matches!(case.context, Context::ChatDrill | Context::TasksReorder);
    for key in &inherited {
        assert!(
            !exclusive || witnessed(*key),
            "{name}: inherited {key:?} has no witness"
        );
    }
    for w in case.inherited {
        assert!(
            w.keys.len() == 1 && inherited.contains(&w.keys[0]),
            "{name}: {:?} is not an inherited row",
            w.keys
        );
    }
}
