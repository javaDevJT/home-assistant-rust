//! Identity policy used by the native Spotify authorization hook.

use hass_config::ConfigEntry;
use hass_oauth2_flow::{OAuthEntryDetails, OAuthFlowError};
use hass_spotify::UserProfile;
use serde_json::{Map, Value};

pub(super) fn entry_details(
    profile: &UserProfile,
    source: &str,
    existing: Option<&ConfigEntry>,
    entries: &[ConfigEntry],
) -> Result<OAuthEntryDetails, OAuthFlowError> {
    let account_id = supplied(profile.account_id.as_deref());
    let legacy_id = supplied(profile.id.as_deref());
    let (preferred, kind) = account_id
        .map(|id| (id, "account_id"))
        .or_else(|| legacy_id.map(|id| (id, "legacy_id")))
        .ok_or(OAuthFlowError::ProviderFailure)?;
    let updating = matches!(source, "reauth" | "reconfigure");
    let existing = if updating {
        Some(existing.ok_or(OAuthFlowError::EntryUnavailable)?)
    } else {
        None
    };
    if let Some(entry) = existing
        && !matches_account(entry, account_id, legacy_id)
    {
        return Err(OAuthFlowError::ProviderAbort(if source == "reauth" {
            "reauth_wrong_account"
        } else {
            "reconfigure_wrong_account"
        }));
    }
    if entries.iter().any(|entry| {
        entry.domain == "spotify"
            && existing.is_none_or(|current| entry.entry_id != current.entry_id)
            && matches_account(entry, account_id, legacy_id)
    }) {
        return Err(OAuthFlowError::ProviderAbort("already_configured"));
    }
    let identity = existing
        .and_then(|entry| entry.unique_id.as_deref())
        .unwrap_or(preferred);
    let identity_kind = existing
        .and_then(|entry| entry.data.get("identity_kind"))
        .and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "account_id" | "legacy_id"))
        .unwrap_or_else(|| match existing {
            Some(entry)
                if entry.data.get("account_id").and_then(Value::as_str) == Some(identity) =>
            {
                "account_id"
            }
            Some(_) => "legacy_id",
            None => kind,
        });
    let name = supplied(profile.display_name.as_deref())
        .or(legacy_id)
        .unwrap_or(preferred);
    let mut data = Map::new();
    if let Some(id) = account_id {
        data.insert("account_id".into(), Value::String(id.into()));
    }
    if let Some(id) = legacy_id {
        data.insert("id".into(), Value::String(id.into()));
    }
    data.insert("name".into(), Value::String(name.into()));
    data.insert("identity_kind".into(), Value::String(identity_kind.into()));
    Ok(OAuthEntryDetails::new(
        name,
        data,
        Map::new(),
        Some(identity.into()),
        (1, 1),
    ))
}

fn supplied(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn matches_account(entry: &ConfigEntry, account_id: Option<&str>, legacy_id: Option<&str>) -> bool {
    let stored_account = entry.data.get("account_id").and_then(Value::as_str);
    let stored_legacy = entry.data.get("id").and_then(Value::as_str);
    account_id.is_some_and(|id| stored_account == Some(id))
        || legacy_id.is_some_and(|id| stored_legacy == Some(id))
        || entry
            .unique_id
            .as_deref()
            .is_some_and(|id| account_id == Some(id) || legacy_id == Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_config::{ConfigEntries, NewConfigEntry};
    use hass_core::{MemoryStore, Runtime};
    use serde_json::json;
    use std::sync::Arc;

    fn profile(account: Option<&str>, legacy: Option<&str>) -> UserProfile {
        UserProfile {
            account_id: account.map(Into::into),
            id: legacy.map(Into::into),
            display_name: Some("Spotify fixture".into()),
            ..UserProfile::default()
        }
    }

    fn stored(unique_id: &str, account: Option<&str>, legacy: Option<&str>) -> ConfigEntry {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let manager = ConfigEntries::new(runtime).unwrap();
        let mut data = Map::new();
        if let Some(account) = account {
            data.insert("account_id".into(), json!(account));
        }
        if let Some(legacy) = legacy {
            data.insert("id".into(), json!(legacy));
        }
        let mut seed = NewConfigEntry::new("spotify", "Original account", data);
        seed.unique_id = Some(unique_id.into());
        manager.create_entry(seed).unwrap()
    }

    #[test]
    fn new_link_prefers_current_identifier_and_stores_only_needed_profile_fields() {
        let mut profile = profile(Some("account-current"), Some("legacy-user"));
        profile.extra.insert("email".into(), json!("private-email"));
        profile
            .extra
            .insert("token".into(), json!("untrusted-profile-token"));
        let details = entry_details(&profile, "user", None, &[]).unwrap();
        assert_eq!(details.unique_id.as_deref(), Some("account-current"));
        assert_eq!(details.data, json!({"account_id":"account-current","id":"legacy-user","name":"Spotify fixture","identity_kind":"account_id"}).as_object().unwrap().clone());
        assert_eq!(details.title, "Spotify fixture");
        assert!(details.options.is_empty());
        let legacy =
            entry_details(&self::profile(None, Some("legacy-user")), "user", None, &[]).unwrap();
        assert_eq!(legacy.unique_id.as_deref(), Some("legacy-user"));
        assert_eq!(legacy.data["identity_kind"], "legacy_id");
        assert!(matches!(
            entry_details(&self::profile(None, None), "user", None, &[]),
            Err(OAuthFlowError::ProviderFailure)
        ));
    }

    #[test]
    fn reauthentication_preserves_legacy_identity_and_rejects_a_different_account() {
        let original = stored("legacy-user", None, Some("legacy-user"));
        let known = profile(Some("current-account"), Some("legacy-user"));
        let details = entry_details(
            &known,
            "reauth",
            Some(&original),
            std::slice::from_ref(&original),
        )
        .unwrap();
        assert_eq!(details.unique_id, original.unique_id);
        assert_eq!(details.data["account_id"], "current-account");
        assert_eq!(details.data["identity_kind"], "legacy_id");
        for (source, reason) in [
            ("reauth", "reauth_wrong_account"),
            ("reconfigure", "reconfigure_wrong_account"),
        ] {
            assert!(
                matches!(entry_details(&profile(Some("different"), Some("other-user")), source, Some(&original), &[]), Err(OAuthFlowError::ProviderAbort(actual)) if actual == reason)
            );
        }
        assert!(matches!(
            entry_details(&known, "reauth", None, &[]),
            Err(OAuthFlowError::EntryUnavailable)
        ));
    }

    #[test]
    fn duplicate_checks_both_known_identifiers_and_preserves_current_identity() {
        let legacy = stored("legacy-user", None, Some("legacy-user"));
        assert!(matches!(
            entry_details(
                &profile(Some("current-account"), Some("legacy-user")),
                "user",
                None,
                &[legacy]
            ),
            Err(OAuthFlowError::ProviderAbort("already_configured"))
        ));
        let current = stored(
            "current-account",
            Some("current-account"),
            Some("legacy-user"),
        );
        let details = entry_details(
            &profile(None, Some("legacy-user")),
            "reauth",
            Some(&current),
            std::slice::from_ref(&current),
        )
        .unwrap();
        assert_eq!(details.unique_id.as_deref(), Some("current-account"));
        assert_eq!(details.data["identity_kind"], "account_id");
        let mut unrelated = current.clone();
        unrelated.domain = "other_provider".into();
        assert!(
            entry_details(
                &profile(Some("current-account"), Some("legacy-user")),
                "user",
                None,
                &[unrelated]
            )
            .is_ok()
        );
    }
}
