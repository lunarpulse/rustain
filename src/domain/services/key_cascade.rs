//! One pure stage walk for provenance, availability and credential construction.
use std::collections::BTreeSet;

use crate::domain::models::{
    AuthSource, Credential, CredentialId, CredentialOrigin, ResolvedAuth, SecretString,
    StoredCredential,
};

pub const OAUTH_REFRESH_SKEW_MS: i64 = 60_000;

pub struct CascadeInputs<'a> {
    pub runtime: Option<&'a SecretString>,
    pub rows: &'a [StoredCredential],
    pub env_vars: &'a [&'a str],
    pub env_lookup: &'a dyn Fn(&str) -> Option<String>,
    pub oauth_allowed: bool,
    pub now_ms: i64,
}

#[derive(Debug)]
pub enum SelectionMaterial {
    Ready(ResolvedAuth),
    NeedsRefresh { id: CredentialId, rev: u64 },
}

#[derive(Debug)]
pub struct Selection<'a> {
    pub source: AuthSource,
    pub material: SelectionMaterial,
    pub row: Option<&'a StoredCredential>,
    pub last_validated: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn oauth_is_fresh(expires_at_ms: i64, now_ms: i64) -> bool {
    now_ms.saturating_add(OAUTH_REFRESH_SKEW_MS) < expires_at_ms
}

pub fn select<'a>(
    inputs: &CascadeInputs<'a>,
    skip: &BTreeSet<CredentialId>,
) -> Option<Selection<'a>> {
    if let Some(runtime) = inputs.runtime {
        return Some(Selection {
            source: AuthSource::Runtime,
            material: SelectionMaterial::Ready(ResolvedAuth::ApiKey(runtime.clone())),
            row: None,
            last_validated: None,
        });
    }
    let eligible =
        |row: &&StoredCredential| row.disabled_cause.is_none() && !skip.contains(&row.id);
    for row in inputs.rows.iter().filter(eligible) {
        if inputs.oauth_allowed
            && let Credential::OAuth(oauth) = &row.credential
        {
            return Some(Selection {
                source: AuthSource::StoredOAuth,
                material: if oauth_is_fresh(oauth.expires_at_ms, inputs.now_ms) {
                    SelectionMaterial::Ready(ResolvedAuth::OAuth {
                        access: oauth.access.clone(),
                        account_id: oauth.identity.account_id.clone(),
                    })
                } else {
                    SelectionMaterial::NeedsRefresh {
                        id: row.id,
                        rev: row.rev,
                    }
                },
                row: Some(row),
                last_validated: row.last_validated,
            });
        }
    }
    let key_row = |origin| {
        inputs.rows.iter().filter(eligible).find_map(|row| {
            if row.origin == origin {
                if let Credential::ApiKey(key) = &row.credential {
                    return Some((row, key));
                }
            }
            None
        })
    };
    let key_selection = |row: &'a StoredCredential, key: &SecretString, source| Selection {
        source,
        material: SelectionMaterial::Ready(ResolvedAuth::ApiKey(key.clone())),
        row: Some(row),
        last_validated: row.last_validated,
    };
    if let Some((row, key)) = key_row(CredentialOrigin::Login) {
        return Some(key_selection(row, key, AuthSource::LoginKey));
    }
    for var in inputs.env_vars.iter().filter(|var| !var.is_empty()) {
        if let Some(value) = (inputs.env_lookup)(var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(Selection {
                    source: AuthSource::Env {
                        var: (*var).to_owned(),
                    },
                    material: SelectionMaterial::Ready(ResolvedAuth::ApiKey(value.into())),
                    row: None,
                    last_validated: None,
                });
            }
        }
    }
    key_row(CredentialOrigin::Stored)
        .map(|(row, key)| key_selection(row, key, AuthSource::StoredKey))
}

/// Synchronous construction cannot refresh; skip stale row IDs, not stages.
pub fn select_ready<'a>(inputs: &CascadeInputs<'a>) -> Option<Selection<'a>> {
    let mut skip = BTreeSet::new();
    loop {
        let selection = select(inputs, &skip)?;
        match selection.material {
            SelectionMaterial::Ready(_) => return Some(selection),
            SelectionMaterial::NeedsRefresh { id, .. } => {
                skip.insert(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{CredentialGeneration, CredentialIdentity, OAuthCredential};

    fn oauth(id: u64, expires_at_ms: i64) -> StoredCredential {
        StoredCredential {
            id: CredentialId(id),
            rev: 1,
            generation: CredentialGeneration([1; 16]),
            origin: CredentialOrigin::Stored,
            credential: Credential::OAuth(OAuthCredential {
                access: format!("access-{id}").into(),
                refresh: "refresh".into(),
                expires_at_ms,
                identity: CredentialIdentity::default(),
                authorized_at_ms: None,
            }),
            last_validated: None,
            disabled_cause: None,
        }
    }

    #[test]
    fn skew_boundary_and_row_scoped_ready_fallthrough() {
        let rows = [oauth(1, 61_000), oauth(2, 61_001)];
        let inputs = CascadeInputs {
            runtime: None,
            rows: &rows,
            env_vars: &["KEY"],
            env_lookup: &|_| Some("env-key".into()),
            oauth_allowed: true,
            now_ms: 1_000,
        };
        assert!(matches!(
            select(&inputs, &BTreeSet::new()).unwrap().material,
            SelectionMaterial::NeedsRefresh {
                id: CredentialId(1),
                rev: 1
            }
        ));
        let selected = select_ready(&inputs).unwrap();
        assert_eq!(selected.source, AuthSource::StoredOAuth);
        assert_eq!(selected.row.unwrap().id, CredentialId(2));
        let SelectionMaterial::Ready(auth) = selected.material else {
            panic!("ready")
        };
        assert_eq!(auth.bearer_secret().expose_secret(), "access-2");
    }

    #[test]
    fn disabled_oauth_and_blank_env_never_serve() {
        let mut row = oauth(1, i64::MAX);
        row.disabled_cause = Some("revoked".into());
        let inputs = CascadeInputs {
            runtime: None,
            rows: &[row],
            env_vars: &["KEY"],
            env_lookup: &|_| Some("  \n".into()),
            oauth_allowed: true,
            now_ms: 0,
        };
        assert!(select(&inputs, &BTreeSet::new()).is_none());
    }
}
