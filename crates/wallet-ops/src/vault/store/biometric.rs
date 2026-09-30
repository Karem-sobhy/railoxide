//! Touch ID unlock keeps a copy of the vault password sealed to a Secure
//! Enclave key that needs a Touch ID match to decrypt. The record itself is not
//! vault-encrypted because it must be readable before unlock; its key handle and
//! ciphertext only work on this Mac, with the fingers enrolled at sealing time.
//! The record is bound to the vault salt, so a password change or a replaced
//! vault marks it stale. Typing the password always keeps working, and the next
//! typed password reseals a stale record.

use serde::{Deserialize, Serialize};

use super::{DesktopVaultStore, SALT_LEN, VaultError, Zeroizing};
use crate::biometric::{self, BiometricError, SealedSecret};

const BIOMETRIC_UNLOCK_KEY: &str = "biometric-unlock|vault-password";
const BIOMETRIC_UNLOCK_VERSION: u32 = 1;
const PASSWORD_LENGTH_PREFIX: usize = 4;
/// Sealed passwords are padded so the ciphertext does not reveal their length.
const PASSWORD_PADDING_BLOCK: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiometricUnlockStatus {
    /// Touch ID is unavailable on this Mac or was never turned on.
    Disabled,
    Enabled,
    /// Touch ID was turned on, but the sealed password no longer matches the
    /// vault or the enrolled fingers. The next verified password reseals it.
    NeedsReenrollment,
}

#[derive(Serialize, Deserialize)]
struct StoredBiometricUnlock {
    version: u32,
    vault_salt: [u8; SALT_LEN],
    domain_state: Option<Vec<u8>>,
    key_handle: Vec<u8>,
    ciphertext: Vec<u8>,
    stale: bool,
}

impl DesktopVaultStore {
    pub fn biometric_unlock_status(&self) -> Result<BiometricUnlockStatus, VaultError> {
        let Some(record) = self.biometric_unlock_record()? else {
            return Ok(BiometricUnlockStatus::Disabled);
        };
        if !biometric::biometric_unlock_available() {
            return Ok(BiometricUnlockStatus::Disabled);
        }
        Ok(if self.biometric_unlock_is_current(&record)? {
            BiometricUnlockStatus::Enabled
        } else {
            BiometricUnlockStatus::NeedsReenrollment
        })
    }

    /// Verifies `password` against the vault and seals it for Touch ID unlock.
    pub fn enable_biometric_unlock(&self, password: &str) -> Result<(), VaultError> {
        self.unlock_view(password)?;
        self.seal_biometric_unlock(password)
    }

    pub fn disable_biometric_unlock(&self) -> Result<(), VaultError> {
        self.db
            .delete_desktop_wallet_vault_record(BIOMETRIC_UNLOCK_KEY)?;
        Ok(())
    }

    /// Reseals a stale Touch ID record with a password the caller has already
    /// verified against the vault. Does nothing unless Touch ID was turned on.
    pub fn renew_biometric_unlock(&self, password: &str) -> Result<(), VaultError> {
        if self.biometric_unlock_status()? == BiometricUnlockStatus::NeedsReenrollment {
            self.seal_biometric_unlock(password)?;
        }
        Ok(())
    }

    /// Prompts for Touch ID with `reason` and returns the sealed vault password.
    ///
    /// Blocks until the prompt is answered. The password is not checked here:
    /// callers pass it to the same vault operations as a typed password.
    pub fn biometric_vault_password(&self, reason: &str) -> Result<Zeroizing<String>, VaultError> {
        let Some(mut record) = self.biometric_unlock_record()? else {
            return Err(VaultError::BiometricUnlockDisabled);
        };
        if !self.biometric_unlock_is_current(&record)? {
            return Err(VaultError::BiometricUnlockDisabled);
        }
        let sealed = SealedSecret {
            key_handle: record.key_handle.clone(),
            ciphertext: record.ciphertext.clone(),
        };
        match biometric::open_secret(&sealed, reason) {
            Ok(padded) => unpad_password(&padded),
            Err(BiometricError::Failed(reason)) => {
                // The Secure Enclave key may be invalidated for good, so fall
                // back to the password once and reseal a fresh key after it.
                record.stale = true;
                self.put_biometric_unlock_record(&record)?;
                Err(BiometricError::Failed(reason).into())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Keeps Touch ID working after the vault password changed.
    pub(super) fn reseal_biometric_unlock_after_password_change(&self, new_password: &str) {
        match self.biometric_unlock_record() {
            Ok(Some(_)) => {
                if let Err(error) = self.seal_biometric_unlock(new_password) {
                    // The old record no longer matches the vault salt, so it is
                    // stale and the next typed password reseals it.
                    tracing::warn!(%error, "failed to reseal Touch ID unlock after password change");
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "failed to read Touch ID unlock after password change");
            }
        }
    }

    fn seal_biometric_unlock(&self, password: &str) -> Result<(), VaultError> {
        let metadata = self.metadata()?;
        let sealed = biometric::seal_secret(&pad_password(password))?;
        self.put_biometric_unlock_record(&StoredBiometricUnlock {
            version: BIOMETRIC_UNLOCK_VERSION,
            vault_salt: metadata.salt,
            domain_state: biometric::biometric_domain_state(),
            key_handle: sealed.key_handle,
            ciphertext: sealed.ciphertext,
            stale: false,
        })
    }

    fn biometric_unlock_is_current(
        &self,
        record: &StoredBiometricUnlock,
    ) -> Result<bool, VaultError> {
        if record.stale
            || record.version != BIOMETRIC_UNLOCK_VERSION
            || record.vault_salt != self.metadata()?.salt
        {
            return Ok(false);
        }
        // The Secure Enclave key only accepts the fingers enrolled when it was
        // made. Skip a prompt that can only fail after enrollment changed.
        Ok(
            match (&record.domain_state, biometric::biometric_domain_state()) {
                (Some(sealed), Some(current)) => *sealed == current,
                _ => true,
            },
        )
    }

    fn biometric_unlock_record(&self) -> Result<Option<StoredBiometricUnlock>, VaultError> {
        self.db
            .get_desktop_wallet_vault_record(BIOMETRIC_UNLOCK_KEY)?
            .map(|data| rmp_serde::from_slice(&data).map_err(VaultError::from))
            .transpose()
    }

    fn put_biometric_unlock_record(
        &self,
        record: &StoredBiometricUnlock,
    ) -> Result<(), VaultError> {
        let data = rmp_serde::to_vec_named(record)?;
        self.db
            .put_desktop_wallet_vault_record(BIOMETRIC_UNLOCK_KEY, &data)?;
        Ok(())
    }
}

fn pad_password(password: &str) -> Zeroizing<Vec<u8>> {
    let bytes = password.as_bytes();
    let padded_len = (PASSWORD_LENGTH_PREFIX + bytes.len()).div_ceil(PASSWORD_PADDING_BLOCK)
        * PASSWORD_PADDING_BLOCK;
    // Reserve the final size up front so no unzeroized reallocation is left behind.
    let mut padded = Zeroizing::new(Vec::with_capacity(padded_len));
    padded.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    padded.extend_from_slice(bytes);
    padded.resize(padded_len, 0);
    padded
}

fn unpad_password(padded: &[u8]) -> Result<Zeroizing<String>, VaultError> {
    let (length, rest) = padded
        .split_first_chunk::<PASSWORD_LENGTH_PREFIX>()
        .ok_or(VaultError::BiometricUnlockCorrupt)?;
    let bytes = rest
        .get(..u32::from_be_bytes(*length) as usize)
        .ok_or(VaultError::BiometricUnlockCorrupt)?;
    let password = std::str::from_utf8(bytes).map_err(|_| VaultError::BiometricUnlockCorrupt)?;
    Ok(Zeroizing::new(password.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{PASSWORD_PADDING_BLOCK, pad_password, unpad_password};

    #[test]
    fn padded_password_round_trips_and_hides_its_length() {
        for password in ["", "a", "correct horse battery staple", "пароль 密码 🔑"] {
            let padded = pad_password(password);
            assert_eq!(padded.len() % PASSWORD_PADDING_BLOCK, 0);
            assert_eq!(unpad_password(&padded).unwrap().as_str(), password);
        }
        assert_eq!(pad_password("a").len(), pad_password(&"a".repeat(60)).len());
        assert_eq!(
            pad_password(&"a".repeat(61)).len(),
            2 * PASSWORD_PADDING_BLOCK
        );
    }

    #[test]
    fn corrupt_padding_is_rejected() {
        assert!(unpad_password(&[0, 0]).is_err());
        assert!(unpad_password(&[0, 0, 0, 9, b'a']).is_err());
        assert!(unpad_password(&[0, 0, 0, 1, 0xff]).is_err());
    }
}
