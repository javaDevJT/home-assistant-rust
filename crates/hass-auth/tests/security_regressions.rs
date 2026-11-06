use hass_auth::{AuthManager, LoginAuthResult};
use hass_core::{MemoryStore, Runtime};
use std::sync::Arc;
use totp_rs::{Algorithm, Builder, Secret};

const PASSWORD: &str = "fixture-strong-password-123456";

#[test]
fn own_profile_preserves_privileges_and_user_events_follow_persisted_changes() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", PASSWORD)
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", PASSWORD, false)
        .unwrap();
    let tokens = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let mut events = runtime.subscribe();
    let updated = auth.update_own_name(&user.id, "  Updated name  ").unwrap();
    assert_eq!(updated.name, "Updated name");
    assert_eq!(updated.group_ids, user.group_ids);
    assert!(!updated.is_admin);
    assert!(!updated.is_owner);
    assert!(updated.is_active);
    assert!(auth.validate_access_token(&tokens.access_token).is_ok());
    let event = events.try_recv().unwrap();
    assert_eq!(event.event_type, "user_updated");
    assert_eq!(
        serde_json::to_value(event.data).unwrap(),
        serde_json::json!({"user_id":user.id})
    );
    assert!(auth.update_own_name(&user.id, " ").is_err());
    assert!(
        auth.update_user(&user.id, &owner.id, Some("Forbidden"), None, None, None)
            .is_err()
    );
    assert!(events.try_recv().is_err());
    drop(auth);
    let auth = AuthManager::new(runtime).unwrap();
    assert_eq!(auth.get_user(&user.id).unwrap().name, "Updated name");
    auth.set_user_active(&owner.id, &user.id, false).unwrap();
    assert_eq!(events.try_recv().unwrap().event_type, "user_updated");
    assert!(auth.update_own_name(&user.id, "Disabled").is_err());
    assert!(events.try_recv().is_err());
    auth.delete_user(&owner.id, &user.id).unwrap();
    let removed = events.try_recv().unwrap();
    assert_eq!(removed.event_type, "user_removed");
    assert_eq!(
        removed
            .data
            .get("user_id")
            .and_then(serde_json::Value::as_str),
        Some(user.id.as_str())
    );
    assert!(auth.get_user(&user.id).is_err());
    assert!(auth.validate_access_token(&tokens.access_token).is_err());
}

#[test]
fn non_owner_admin_cannot_revoke_peer_admin_sessions() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", PASSWORD)
        .unwrap();
    let first = auth
        .create_user_with_username(&owner.id, "admin_a", "Admin A", PASSWORD, true)
        .unwrap();
    let second = auth
        .create_user_with_username(&owner.id, "admin_b", "Admin B", PASSWORD, true)
        .unwrap();
    let tokens = auth
        .issue_tokens(&second.id, "http://localhost/", None)
        .unwrap();
    assert!(auth.revoke_user_sessions(&first.id, &second.id).is_err());
    assert!(auth.validate_access_token(&tokens.access_token).is_ok());
    assert!(auth.revoke_user_sessions(&owner.id, &second.id).unwrap() > 0);
    assert!(auth.validate_access_token(&tokens.access_token).is_err());
}

#[test]
fn mfa_user_budget_survives_new_challenges_and_manager_restart() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", PASSWORD)
        .unwrap();
    let enrollment = auth.begin_mfa_enrollment(&owner.id).unwrap();
    let totp = Builder::new()
        .with_secret(Secret::try_from_base32(&enrollment.secret).unwrap())
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .build()
        .unwrap();
    let now = chrono::Utc::now().timestamp() as u64;
    let confirmation = totp.generate(now).to_string();
    auth.confirm_mfa_enrollment(&owner.id, &confirmation)
        .unwrap();
    for _ in 0..5 {
        let LoginAuthResult::MfaRequired { challenge_id, .. } =
            auth.begin_mfa_login("owner", PASSWORD).unwrap()
        else {
            panic!("MFA must be required");
        };
        let now = chrono::Utc::now().timestamp() as u64;
        let candidates = [
            totp.generate(now.saturating_sub(30)).to_string(),
            totp.generate(now).to_string(),
            totp.generate(now + 30).to_string(),
        ];
        let wrong = (0..4)
            .map(|value| format!("{value:06}"))
            .find(|value| !candidates.contains(value))
            .unwrap();
        assert!(auth.complete_mfa_login(&challenge_id, &wrong).is_err());
    }
    assert!(auth.begin_mfa_login("owner", PASSWORD).is_err());
    drop(auth);
    let restored = AuthManager::new(runtime).unwrap();
    assert!(restored.begin_mfa_login("owner", PASSWORD).is_err());
}
