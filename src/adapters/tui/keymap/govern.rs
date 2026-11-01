//! Row-level governance next to `resolve::validate` (Story 21.3 AC1, AC7): every
//! row says in plain words what it does, and no sequence row is shadowed by a
//! shorter row of the same context (`g` hides `g g`).

use super::model::{Binding, Context, Host, Key};

/// One row-level defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowDefect {
    /// The row has no `does`, no help text and no label: the generated reference
    /// would have nothing to say (it must never print an enum name).
    NoDescription { context: Context, keys: Vec<Key> },
    /// `shorter` resolves first, so `longer` can never complete.
    ShadowedSequence {
        context: Context,
        shorter: Vec<Key>,
        longer: Vec<Key>,
    },
}

/// What the generated reference says the row does: its `does` text, else its
/// help text, else its label.
pub fn description(row: &Binding) -> Option<&'static str> {
    [row.does, row.help.map_or("", |help| help.desc), row.label]
        .into_iter()
        .find(|text| !text.is_empty())
}

/// Reject rows without a description and shadowed sequences.
pub fn validate_rows(rows: &[Binding]) -> Result<(), Vec<RowDefect>> {
    let mut defects = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if description(row).is_none() {
            defects.push(RowDefect::NoDescription {
                context: row.context,
                keys: row.keys.to_vec(),
            });
        }
        let fold = row.context.folds_case();
        let keys: Vec<Key> = row.keys.iter().map(|k| k.normalized(fold)).collect();
        for (other_index, other) in rows.iter().enumerate() {
            let overlap = [Host::Local, Host::Attached]
                .iter()
                .any(|h| row.hosts.includes(*h) && other.hosts.includes(*h));
            if index == other_index || other.context != row.context || !overlap {
                continue;
            }
            let other_keys: Vec<Key> = other.keys.iter().map(|k| k.normalized(fold)).collect();
            if keys.len() < other_keys.len() && other_keys[..keys.len()] == keys[..] {
                defects.push(RowDefect::ShadowedSequence {
                    context: row.context,
                    shorter: keys.clone(),
                    longer: other_keys,
                });
            }
        }
    }
    if defects.is_empty() {
        Ok(())
    } else {
        Err(defects)
    }
}
