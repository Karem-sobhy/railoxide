//! Touch ID sealing for small secrets such as the vault password.
//!
//! A secret is encrypted to a Secure Enclave P-256 key whose private half can
//! only be used after a Touch ID match with the fingers enrolled when the key
//! was created. The private key never leaves the Secure Enclave: callers store
//! only its opaque handle and the ECIES ciphertext, and both are useless on any
//! other Mac. Platforms without Touch ID report [`BiometricError::Unavailable`].

use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum BiometricError {
    #[error("Touch ID is not available on this device")]
    Unavailable,
    #[error("Touch ID was cancelled")]
    Cancelled,
    #[error("Touch ID failed: {0}")]
    Failed(String),
}

/// A secret sealed to a Touch ID protected Secure Enclave key.
pub struct SealedSecret {
    /// Opaque Secure Enclave key handle. It only works on the Mac that made it.
    pub key_handle: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Whether Touch ID is present, enrolled, and not locked out.
#[must_use]
pub fn biometric_unlock_available() -> bool {
    platform::available()
}

/// An opaque value that changes whenever enrolled fingers are added or removed.
#[must_use]
pub fn biometric_domain_state() -> Option<Vec<u8>> {
    platform::domain_state()
}

/// Seals `secret` without prompting. Only [`open_secret`] needs a finger.
pub fn seal_secret(secret: &[u8]) -> Result<SealedSecret, BiometricError> {
    platform::seal(secret)
}

/// Prompts for Touch ID with `reason` and returns the sealed secret.
///
/// Blocks the calling thread until the prompt is answered, so call it off the
/// UI thread.
pub fn open_secret(
    sealed: &SealedSecret,
    reason: &str,
) -> Result<Zeroizing<Vec<u8>>, BiometricError> {
    platform::open(sealed, reason)
}

#[cfg(target_os = "macos")]
mod platform {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::error::CFError;
    use core_foundation::string::CFString;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::{NSData, NSString};
    use security_framework::access_control::{ProtectionMode, SecAccessControl};
    use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey, Token};
    use security_framework_sys::access_control::{
        kSecAccessControlBiometryCurrentSet, kSecAccessControlPrivateKeyUsage,
    };
    use security_framework_sys::item::{
        kSecAttrKeyClass, kSecAttrKeyClassPrivate, kSecAttrKeyType,
        kSecAttrKeyTypeECSECPrimeRandom, kSecAttrTokenID, kSecAttrTokenIDSecureEnclave,
        kSecUseAuthenticationContext,
    };
    use security_framework_sys::key::SecKeyCreateWithData;
    use zeroize::Zeroizing;

    use super::{BiometricError, SealedSecret};

    #[link(name = "LocalAuthentication", kind = "framework")]
    unsafe extern "C" {}

    /// `LAPolicyDeviceOwnerAuthenticationWithBiometrics`
    const LA_POLICY_BIOMETRICS: isize = 1;
    const ALGORITHM: Algorithm = Algorithm::ECIESEncryptionCofactorVariableIVX963SHA256AESGCM;
    /// `kSecAttrTokenOID`: the Secure Enclave key handle. The SDK does not export
    /// the constant, but `SecKeyCopyAttributes` returns it under this name and
    /// `SecKeyCreateWithData` accepts it to reopen the key.
    const TOKEN_OBJECT_ID: &str = "toid";
    const LA_ERROR_DOMAIN: &str = "com.apple.LocalAuthentication";
    const LA_CANCEL_CODES: [isize; 4] = [
        -2, // LAErrorUserCancel
        -3, // LAErrorUserFallback
        -4, // LAErrorSystemCancel
        -9, // LAErrorAppCancel
    ];
    const OS_STATUS_ERROR_DOMAIN: &str = "NSOSStatusErrorDomain";
    const ERR_SEC_USER_CANCELED: isize = -128;
    const TOKEN_ERROR_DOMAIN: &str = "CryptoTokenKit";
    const TOKEN_CANCELED_BY_USER: isize = -9;

    fn la_context() -> Option<Retained<AnyObject>> {
        let class = AnyClass::get(c"LAContext")?;
        Some(unsafe { msg_send![class, new] })
    }

    fn can_evaluate_biometrics(context: &AnyObject) -> bool {
        unsafe {
            msg_send![
                context,
                canEvaluatePolicy: LA_POLICY_BIOMETRICS,
                error: std::ptr::null_mut::<*mut AnyObject>()
            ]
        }
    }

    pub(super) fn available() -> bool {
        la_context().is_some_and(|context| can_evaluate_biometrics(&context))
    }

    pub(super) fn domain_state() -> Option<Vec<u8>> {
        let context = la_context()?;
        // The state is only populated after a policy check, whatever its result.
        can_evaluate_biometrics(&context);
        let state: Option<Retained<NSData>> =
            unsafe { msg_send![&*context, evaluatedPolicyDomainState] };
        state.map(|state| state.to_vec())
    }

    pub(super) fn seal(secret: &[u8]) -> Result<SealedSecret, BiometricError> {
        if !available() {
            return Err(BiometricError::Unavailable);
        }
        let access_control = SecAccessControl::create_with_protection(
            Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
            kSecAccessControlBiometryCurrentSet | kSecAccessControlPrivateKeyUsage,
        )
        .map_err(|error| BiometricError::Failed(error.to_string()))?;
        let mut options = GenerateKeyOptions::default();
        options
            .set_key_type(KeyType::ec_sec_prime_random())
            .set_size_in_bits(256)
            .set_token(Token::SecureEnclave)
            .set_access_control(access_control);
        // No location: the key stays out of the keychain, which would need
        // entitlements that source builds do not have.
        let key = SecKey::new(&options).map_err(|error| failed(&error))?;
        let key_handle = key
            .attributes()
            .find(CFString::from_static_string(TOKEN_OBJECT_ID).as_CFTypeRef())
            .map(|handle| unsafe { CFData::wrap_under_get_rule(handle.cast()) }.to_vec())
            .ok_or_else(|| BiometricError::Failed("Secure Enclave key handle missing".into()))?;
        let ciphertext = key
            .public_key()
            .ok_or_else(|| BiometricError::Failed("Secure Enclave public key missing".into()))?
            .encrypt_data(ALGORITHM, secret)
            .map_err(|error| failed(&error))?;
        Ok(SealedSecret {
            key_handle,
            ciphertext,
        })
    }

    pub(super) fn open(
        sealed: &SealedSecret,
        reason: &str,
    ) -> Result<Zeroizing<Vec<u8>>, BiometricError> {
        let context = la_context().ok_or(BiometricError::Unavailable)?;
        if !can_evaluate_biometrics(&context) {
            return Err(BiometricError::Unavailable);
        }
        let reason = NSString::from_str(reason);
        let no_fallback = NSString::from_str("");
        unsafe {
            let () = msg_send![&*context, setLocalizedReason: &*reason];
            let () = msg_send![&*context, setLocalizedFallbackTitle: &*no_fallback];
        }
        let key = reopen_private_key(&sealed.key_handle, &context)?;
        key.decrypt_data(ALGORITHM, &sealed.ciphertext)
            .map(Zeroizing::new)
            .map_err(|error| {
                if is_cancel(&error) {
                    BiometricError::Cancelled
                } else {
                    failed(&error)
                }
            })
    }

    fn reopen_private_key(handle: &[u8], context: &AnyObject) -> Result<SecKey, BiometricError> {
        let string = |value| unsafe { CFString::wrap_under_get_rule(value) };
        // LAContext is an Objective-C object, which CoreFoundation can retain.
        let context =
            unsafe { CFType::wrap_under_get_rule(std::ptr::from_ref(context) as CFTypeRef) };
        let handle = CFData::from_buffer(handle);
        let attributes = CFDictionary::from_CFType_pairs(&[
            (
                string(unsafe { kSecAttrTokenID }),
                string(unsafe { kSecAttrTokenIDSecureEnclave }).as_CFType(),
            ),
            (
                CFString::from_static_string(TOKEN_OBJECT_ID),
                handle.as_CFType(),
            ),
            (
                string(unsafe { kSecAttrKeyType }),
                string(unsafe { kSecAttrKeyTypeECSECPrimeRandom }).as_CFType(),
            ),
            (
                string(unsafe { kSecAttrKeyClass }),
                string(unsafe { kSecAttrKeyClassPrivate }).as_CFType(),
            ),
            (string(unsafe { kSecUseAuthenticationContext }), context),
        ]);
        let mut error = std::ptr::null_mut();
        let key = unsafe {
            SecKeyCreateWithData(
                handle.as_concrete_TypeRef(),
                attributes.as_concrete_TypeRef(),
                &raw mut error,
            )
        };
        if key.is_null() {
            return Err(if error.is_null() {
                BiometricError::Failed("Secure Enclave key could not be opened".into())
            } else {
                failed(&unsafe { CFError::wrap_under_create_rule(error) })
            });
        }
        Ok(unsafe { SecKey::wrap_under_create_rule(key) })
    }

    fn is_cancel(error: &CFError) -> bool {
        let domain = error.domain().to_string();
        let code = error.code();
        (domain == LA_ERROR_DOMAIN && LA_CANCEL_CODES.contains(&code))
            || (domain == OS_STATUS_ERROR_DOMAIN && code == ERR_SEC_USER_CANCELED)
            || (domain == TOKEN_ERROR_DOMAIN && code == TOKEN_CANCELED_BY_USER)
    }

    fn failed(error: &CFError) -> BiometricError {
        BiometricError::Failed(format!(
            "{} ({} {})",
            error.description(),
            error.domain(),
            error.code()
        ))
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    // Not `const`, so the public wrappers keep the same signature on every platform.
    #![allow(clippy::missing_const_for_fn)]

    use zeroize::Zeroizing;

    use super::{BiometricError, SealedSecret};

    pub(super) fn available() -> bool {
        false
    }

    pub(super) fn domain_state() -> Option<Vec<u8>> {
        None
    }

    pub(super) fn seal(_secret: &[u8]) -> Result<SealedSecret, BiometricError> {
        Err(BiometricError::Unavailable)
    }

    pub(super) fn open(
        _sealed: &SealedSecret,
        _reason: &str,
    ) -> Result<Zeroizing<Vec<u8>>, BiometricError> {
        Err(BiometricError::Unavailable)
    }
}
