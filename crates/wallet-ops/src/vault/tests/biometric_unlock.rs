use super::super::*;
use super::helpers::*;
use crate::biometric::biometric_unlock_available;
use std::fs;

#[test]
fn biometric_unlock_is_disabled_until_enabled_and_rejects_wrong_passwords() {
    let (root_dir, _db, store) = desktop_store_with_vault();

    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::Disabled
    );
    assert!(matches!(
        store.biometric_vault_password("test"),
        Err(VaultError::BiometricUnlockDisabled)
    ));
    assert!(matches!(
        store.enable_biometric_unlock("wrong password"),
        Err(VaultError::UnlockFailed)
    ));
    // Renewing never turns Touch ID on by itself.
    store.renew_biometric_unlock(TEST_PASSWORD).expect("renew");
    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::Disabled
    );

    fs::remove_dir_all(root_dir).expect("cleanup");
}

/// Sealing uses the real Secure Enclave but never prompts; only opening does.
#[test]
fn biometric_unlock_survives_password_changes_until_disabled() {
    let (root_dir, _db, store) = desktop_store_with_vault();
    let result = store.enable_biometric_unlock(TEST_PASSWORD);
    if !biometric_unlock_available() {
        assert!(matches!(
            result,
            Err(VaultError::Biometric(
                crate::biometric::BiometricError::Unavailable
            ))
        ));
        fs::remove_dir_all(root_dir).expect("cleanup");
        return;
    }
    result.expect("enable Touch ID");
    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::Enabled
    );

    store
        .reencrypt_vault(TEST_PASSWORD, "new password")
        .expect("change password");
    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::Enabled,
        "a password change reseals the new password"
    );

    let created = create_with_params("replaced vault", test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("replace vault");
    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::NeedsReenrollment,
        "a replaced vault invalidates the sealed password"
    );
    store
        .renew_biometric_unlock("replaced vault")
        .expect("renew");
    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::Enabled
    );

    store.disable_biometric_unlock().expect("disable");
    assert_eq!(
        store.biometric_unlock_status().expect("status"),
        BiometricUnlockStatus::Disabled
    );

    fs::remove_dir_all(root_dir).expect("cleanup");
}
