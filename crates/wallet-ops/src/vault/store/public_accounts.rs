use super::{
    ConfirmedHardwarePublicAccount, DesktopVaultStore, DesktopViewSession, EncryptedRecord,
    KEY_LEN, MAX_DERIVED_ADDRESS_BATCH_COUNT, PUBLIC_ACCOUNT_METADATA_PREFIX,
    ProtectedSoftwareSeedSession, PublicAccountMetadata, PublicAccountScope, PublicAccountSecret,
    PublicAccountSource, PublicAccountStatus, SoftwareSeedSessionBinding, SpendGrant, VaultError,
    ViewUnlock, WalletSoftwareContextKind, Zeroizing, derive_public_evm_address_from_entropy,
    derive_public_evm_address_from_seed, derive_public_evm_private_key_from_entropy,
    derive_public_evm_private_key_from_seed, ensure_public_account_address_available,
    generate_opaque_id, next_derived_public_account_index, next_public_account_display_order,
    next_public_account_label_number, normalize_public_account_label, parse_public_evm_private_key,
    public_account_default_label, public_account_metadata_record_entry,
    public_account_metadata_record_key, public_account_secret_record_entry,
    public_account_secret_record_key, public_evm_address_from_private_key,
    sort_public_account_metadata, unlock_spend, unlock_view, validate_derived_address_page,
    validate_derived_address_range, wallet_spend_record_key,
};
use alloy::primitives::Address;
use std::collections::BTreeSet;

/// One derived address previewed from the current private wallet's key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedAddressPreview {
    pub derivation_index: u32,
    pub address: Address,
}

/// Outcome of a batch derived-account insert. Indexes already present (by
/// derivation index or active address) are skipped, never duplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedBatchAddOutcome {
    pub added: Vec<PublicAccountMetadata>,
    pub skipped_indexes: Vec<u32>,
}

impl DerivedBatchAddOutcome {
    #[must_use]
    pub fn summary(&self) -> String {
        let total = self.added.len() + self.skipped_indexes.len();
        format!(
            "{total} addresses processed — {} added, {} already existed.",
            self.added.len(),
            self.skipped_indexes.len()
        )
    }
}

impl DesktopVaultStore {
    pub fn list_active_public_accounts_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        self.list_public_accounts_for_session(view_session, false)
    }

    pub fn list_public_accounts_for_session(
        &self,
        view_session: &DesktopViewSession,
        include_inactive: bool,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let wallet_id = view_session.wallet_id();
        accounts.retain(|account| {
            account.is_scoped_to_wallet(wallet_id)
                && (include_inactive || account.status == PublicAccountStatus::Active)
        });
        sort_public_account_metadata(&mut accounts);
        Ok(accounts)
    }

    /// Every public account in the vault, regardless of wallet scope; for
    /// cross-wallet display lookups only.
    pub fn list_all_public_accounts(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        sort_public_account_metadata(&mut accounts);
        Ok(accounts)
    }

    pub fn next_derived_public_account_index_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<u32, VaultError> {
        let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        next_derived_public_account_index(&accounts, view_session.wallet_id())
    }

    pub fn add_derived_public_account(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        label: Option<&str>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        self.add_derived_public_account_with_session(password, view_session, label, None)
    }

    pub fn add_derived_public_account_with_session(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        label: Option<&str>,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let mut grant = SpendGrant::one_use(unlock_spend(&vault_metadata, password)?);
        let wallet_id = view_session.wallet_id();
        let accounts = self.list_public_account_metadata_with_view(&view)?;
        let derivation_index = next_derived_public_account_index(&accounts, wallet_id)?;
        let metadata = self.load_wallet_metadata_with_view(&view, wallet_id)?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        let address = match context.kind {
            WalletSoftwareContextKind::Passphrase => {
                let session =
                    protected_seed_session.ok_or(VaultError::SoftwareSeedSessionRequired)?;
                let binding = SoftwareSeedSessionBinding::new(
                    &context.base_profile_uuid,
                    wallet_id,
                    session.binding().vault_session_id(),
                );
                let seed = session.open(&mut grant, &binding)?;
                derive_public_evm_address_from_seed(&seed, derivation_index)?
            }
            WalletSoftwareContextKind::Standard => {
                let spend = grant.take_spend_unlock()?;
                let spend_record = self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
                let spend_bundle = spend.decrypt_spend_bundle(wallet_id, &spend_record)?;
                derive_public_evm_address_from_entropy(
                    &spend_bundle.bip39_entropy,
                    derivation_index,
                )?
            }
        };
        ensure_public_account_address_available(
            &accounts,
            address,
            &PublicAccountScope::PrivateWallet {
                wallet_uuid: wallet_id.to_owned(),
            },
            wallet_id,
        )?;

        let account = PublicAccountMetadata {
            public_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: PublicAccountSource::Derived,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: wallet_id.to_owned(),
            },
            derivation_index: Some(derivation_index),
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: next_public_account_display_order(&accounts)?,
        };
        let (key, data) = public_account_metadata_record_entry(&view, &account)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(account)
    }

    /// Derive preview addresses for explicit derivation indexes from the
    /// current private wallet's key material. Nothing is persisted. The
    /// password unlock is the same one used by single-address derivation, so
    /// no new key source is introduced.
    pub fn preview_derived_public_addresses(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        indexes: &[u32],
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Vec<DerivedAddressPreview>, VaultError> {
        if indexes.is_empty() || indexes.len() > MAX_DERIVED_ADDRESS_BATCH_COUNT as usize {
            return Err(VaultError::InvalidDerivedAddressRange);
        }
        let mut seen = BTreeSet::new();
        for index in indexes {
            if !seen.insert(*index) {
                return Err(VaultError::InvalidDerivedAddressRange);
            }
        }
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let mut grant = SpendGrant::one_use(unlock_spend(&vault_metadata, password)?);
        let wallet_id = view_session.wallet_id();
        let metadata = self.load_wallet_metadata_with_view(&view, wallet_id)?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        match context.kind {
            WalletSoftwareContextKind::Passphrase => {
                let session =
                    protected_seed_session.ok_or(VaultError::SoftwareSeedSessionRequired)?;
                let binding = SoftwareSeedSessionBinding::new(
                    &context.base_profile_uuid,
                    wallet_id,
                    session.binding().vault_session_id(),
                );
                let seed = session.open(&mut grant, &binding)?;
                indexes
                    .iter()
                    .map(|index| {
                        derive_public_evm_address_from_seed(&seed, *index).map(|address| {
                            DerivedAddressPreview {
                                derivation_index: *index,
                                address,
                            }
                        })
                    })
                    .collect()
            }
            WalletSoftwareContextKind::Standard => {
                let spend = grant.take_spend_unlock()?;
                let spend_record = self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
                let spend_bundle = spend.decrypt_spend_bundle(wallet_id, &spend_record)?;
                indexes
                    .iter()
                    .map(|index| {
                        derive_public_evm_address_from_entropy(&spend_bundle.bip39_entropy, *index)
                            .map(|address| DerivedAddressPreview {
                                derivation_index: *index,
                                address,
                            })
                    })
                    .collect()
            }
        }
    }

    /// Preview one browse page (1-indexed, [`DERIVED_ADDRESS_BROWSE_PAGE_SIZE`]
    /// addresses) from the current private wallet's key material.
    pub fn preview_derived_public_address_page(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        page: u32,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Vec<DerivedAddressPreview>, VaultError> {
        let indexes = validate_derived_address_page(page)?;
        self.preview_derived_public_addresses(
            password,
            view_session,
            &indexes,
            protected_seed_session,
        )
    }

    /// Preview an explicit `start..start+count` range from the current private
    /// wallet's key material.
    pub fn preview_derived_public_address_range(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        start: u32,
        count: u32,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Vec<DerivedAddressPreview>, VaultError> {
        let indexes = validate_derived_address_range(start, count)?;
        self.preview_derived_public_addresses(
            password,
            view_session,
            &indexes,
            protected_seed_session,
        )
    }

    /// Add derived accounts at explicit derivation indexes, skipping any index
    /// or address that is already present. Never creates duplicates. Returns
    /// the inserted accounts plus the skipped indexes so the UI can report
    /// e.g. "5 addresses processed — 3 added, 2 already existed."
    pub fn add_derived_public_accounts_at_indexes(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        indexes: &[u32],
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<DerivedBatchAddOutcome, VaultError> {
        if indexes.is_empty() || indexes.len() > MAX_DERIVED_ADDRESS_BATCH_COUNT as usize {
            return Err(VaultError::InvalidDerivedAddressRange);
        }
        // De-duplicate the request itself while preserving ascending order for
        // deterministic labels and display orders.
        let mut ordered: Vec<u32> = indexes.to_vec();
        ordered.sort_unstable();
        ordered.dedup();
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let mut grant = SpendGrant::one_use(unlock_spend(&vault_metadata, password)?);
        let wallet_id = view_session.wallet_id();
        let accounts = self.list_public_account_metadata_with_view(&view)?;
        let metadata = self.load_wallet_metadata_with_view(&view, wallet_id)?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        // Snapshot of what already exists in this wallet's scope: any derived
        // index (active or inactive) blocks re-insertion of that index, and any
        // active address blocks its address (covers imported duplicates).
        let existing_indexes: BTreeSet<u32> = accounts
            .iter()
            .filter(|account| {
                matches!(
                    account.source,
                    PublicAccountSource::Derived | PublicAccountSource::HardwareDerived
                ) && matches!(
                    &account.scope,
                    PublicAccountScope::PrivateWallet { wallet_uuid: scoped }
                    if scoped == wallet_id
                )
            })
            .filter_map(|account| account.derivation_index)
            .collect();
        let active_addresses: BTreeSet<Address> = accounts
            .iter()
            .filter(|account| account.is_active_for_wallet(wallet_id))
            .map(|account| account.address)
            .collect();

        // Derive every requested address up front from the same key material
        // the single-address flow uses (one unlock, one seed/entropy open).
        let previews: Vec<DerivedAddressPreview> = match context.kind {
            WalletSoftwareContextKind::Passphrase => {
                let session =
                    protected_seed_session.ok_or(VaultError::SoftwareSeedSessionRequired)?;
                let binding = SoftwareSeedSessionBinding::new(
                    &context.base_profile_uuid,
                    wallet_id,
                    session.binding().vault_session_id(),
                );
                let seed = session.open(&mut grant, &binding)?;
                let mut out = Vec::with_capacity(ordered.len());
                for index in &ordered {
                    out.push(DerivedAddressPreview {
                        derivation_index: *index,
                        address: derive_public_evm_address_from_seed(&seed, *index)?,
                    });
                }
                out
            }
            WalletSoftwareContextKind::Standard => {
                let spend = grant.take_spend_unlock()?;
                let spend_record = self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
                let spend_bundle = spend.decrypt_spend_bundle(wallet_id, &spend_record)?;
                let mut out = Vec::with_capacity(ordered.len());
                for index in &ordered {
                    out.push(DerivedAddressPreview {
                        derivation_index: *index,
                        address: derive_public_evm_address_from_entropy(
                            &spend_bundle.bip39_entropy,
                            *index,
                        )?,
                    });
                }
                out
            }
        };

        let mut skipped_indexes = Vec::new();
        let mut fresh: Vec<DerivedAddressPreview> = Vec::with_capacity(previews.len());
        // Addresses derived later in the same batch must also collide with
        // earlier ones in the batch, not just pre-existing accounts.
        let mut batch_addresses = active_addresses;
        for preview in previews {
            if existing_indexes.contains(&preview.derivation_index)
                || !batch_addresses.insert(preview.address)
            {
                skipped_indexes.push(preview.derivation_index);
            } else {
                fresh.push(preview);
            }
        }

        let mut added = Vec::with_capacity(fresh.len());
        if !fresh.is_empty() {
            let mut next_display_order = next_public_account_display_order(&accounts)?;
            let mut next_label = next_public_account_label_number(&accounts, wallet_id);
            let mut entries = Vec::with_capacity(fresh.len());
            for preview in fresh {
                let label = public_account_default_label(next_label);
                next_label = next_label.saturating_add(1);
                let account = PublicAccountMetadata {
                    public_account_uuid: generate_opaque_id()?,
                    address: preview.address,
                    label: normalize_public_account_label(Some(&label)),
                    source: PublicAccountSource::Derived,
                    scope: PublicAccountScope::PrivateWallet {
                        wallet_uuid: wallet_id.to_owned(),
                    },
                    derivation_index: Some(preview.derivation_index),
                    hardware_descriptor: None,
                    status: PublicAccountStatus::Active,
                    display_order: next_display_order,
                };
                next_display_order = next_display_order
                    .checked_add(1)
                    .ok_or(VaultError::PublicAccountDisplayOrderOverflow)?;
                let entry = public_account_metadata_record_entry(&view, &account)?;
                entries.push(entry);
                added.push(account);
            }
            skipped_indexes.sort_unstable();
            self.db.put_desktop_wallet_vault_records(&entries)?;
        } else {
            skipped_indexes.sort_unstable();
        }
        Ok(DerivedBatchAddOutcome {
            added,
            skipped_indexes,
        })
    }

    /// Add an explicit `start..start+count` range, skipping existing entries.
    pub fn add_derived_public_accounts_in_range(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        start: u32,
        count: u32,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<DerivedBatchAddOutcome, VaultError> {
        let indexes = validate_derived_address_range(start, count)?;
        self.add_derived_public_accounts_at_indexes(
            password,
            view_session,
            &indexes,
            protected_seed_session,
        )
    }

    pub fn add_hardware_public_account(
        &self,
        view_session: &DesktopViewSession,
        confirmed_account: &ConfirmedHardwarePublicAccount,
        label: Option<&str>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let descriptor = confirmed_account.descriptor().clone();
        let address = confirmed_account.address();
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let wallet_id = view_session.wallet_id();
        let hardware_session = view_session
            .hardware_profile_session()
            .ok_or(VaultError::HardwareWalletViewRequiresDevice)?;
        let wallet_metadata = self.load_wallet_metadata_with_view(&view_session.view, wallet_id)?;
        let hardware_account = wallet_metadata
            .hardware_account
            .as_ref()
            .ok_or(VaultError::HardwareWalletViewRequiresDevice)?;
        Self::ensure_supported_hardware_account(hardware_account)?;
        hardware_session.verify_account(hardware_account)?;
        if descriptor.device_kind != hardware_account.descriptor.device_kind {
            return Err(VaultError::InvalidHardwareWalletDescriptor);
        }
        let derivation_index = next_derived_public_account_index(&accounts, wallet_id)?;
        if descriptor.wallet_account_index != view_session.derivation_index()
            || descriptor.public_account_index != derivation_index
        {
            return Err(VaultError::InvalidHardwareWalletDescriptor);
        }
        let scope = PublicAccountScope::PrivateWallet {
            wallet_uuid: wallet_id.to_owned(),
        };
        ensure_public_account_address_available(&accounts, address, &scope, wallet_id)?;

        let account = PublicAccountMetadata {
            public_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: PublicAccountSource::HardwareDerived,
            scope,
            derivation_index: Some(derivation_index),
            hardware_descriptor: Some(descriptor),
            status: PublicAccountStatus::Active,
            display_order: next_public_account_display_order(&accounts)?,
        };
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &account)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(account)
    }

    pub fn import_public_account(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        private_key_hex: &str,
        label: Option<&str>,
        global: bool,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let spend = unlock_spend(&vault_metadata, password)?;
        let private_key = parse_public_evm_private_key(private_key_hex)?;
        let address = public_evm_address_from_private_key(&private_key)?;
        let accounts = self.list_public_account_metadata_with_view(&view)?;
        let scope = if global {
            PublicAccountScope::Global
        } else {
            PublicAccountScope::PrivateWallet {
                wallet_uuid: view_session.wallet_id().to_owned(),
            }
        };
        ensure_public_account_address_available(
            &accounts,
            address,
            &scope,
            view_session.wallet_id(),
        )?;

        let account = PublicAccountMetadata {
            public_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: PublicAccountSource::Imported,
            scope,
            derivation_index: None,
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: next_public_account_display_order(&accounts)?,
        };
        let secret = PublicAccountSecret {
            private_key: *private_key,
        };
        let metadata_entry = public_account_metadata_record_entry(&view, &account)?;
        let secret_entry = public_account_secret_record_entry(&spend, &account, &secret)?;
        self.db
            .put_desktop_wallet_vault_records(&[metadata_entry, secret_entry])?;
        Ok(account)
    }

    pub fn update_public_account_label(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
        label: Option<&str>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account) = accounts
            .iter_mut()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if !account.is_scoped_to_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        account.label = normalize_public_account_label(label);
        let updated = account.clone();
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn deactivate_derived_public_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account) = accounts
            .iter_mut()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if !account.is_active_for_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        if !matches!(
            account.source,
            PublicAccountSource::Derived
                | PublicAccountSource::HardwareDerived
                | PublicAccountSource::ExecutorDerived(_)
        ) {
            return Err(VaultError::InvalidPublicAccountOperation);
        }
        account.status = PublicAccountStatus::Inactive;
        let updated = account.clone();
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn activate_derived_public_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account_index) = accounts
            .iter()
            .position(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        let account = &accounts[account_index];
        if !account.is_scoped_to_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        if !matches!(
            account.source,
            PublicAccountSource::Derived
                | PublicAccountSource::HardwareDerived
                | PublicAccountSource::ExecutorDerived(_)
        ) {
            return Err(VaultError::InvalidPublicAccountOperation);
        }
        if account.status == PublicAccountStatus::Inactive {
            ensure_public_account_address_available(
                &accounts,
                account.address,
                &account.scope,
                view_session.wallet_id(),
            )?;
            accounts[account_index].status = PublicAccountStatus::Active;
        }
        let updated = accounts[account_index].clone();
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn delete_imported_public_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account) = accounts
            .into_iter()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if !account.is_active_for_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        if account.source != PublicAccountSource::Imported {
            return Err(VaultError::InvalidPublicAccountOperation);
        }

        self.db
            .delete_desktop_wallet_vault_record(&public_account_metadata_record_key(
                &account.public_account_uuid,
            ))?;
        self.db
            .delete_desktop_wallet_vault_record(&public_account_secret_record_key(
                &account.public_account_uuid,
            ))?;
        Ok(account)
    }

    pub fn public_account_signing_key(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        self.public_account_signing_key_with_session(grant, view_session, public_account_uuid, None)
    }

    pub fn public_account_signing_key_with_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        let accounts = self.list_public_accounts_for_session(view_session, true)?;
        let Some(account) = accounts
            .into_iter()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        match account.source {
            PublicAccountSource::Derived => {
                let Some(derivation_index) = account.derivation_index else {
                    return Err(VaultError::InvalidPublicAccountOperation);
                };
                let wallet_id = view_session.wallet_id();
                let metadata =
                    self.load_wallet_metadata_with_view(&view_session.view, wallet_id)?;
                let Some(context) = metadata.software_context.as_ref() else {
                    return Err(VaultError::InvalidWalletMetadata);
                };
                match context.kind {
                    WalletSoftwareContextKind::Passphrase => {
                        let session = protected_seed_session
                            .ok_or(VaultError::SoftwareSeedSessionRequired)?;
                        let binding = SoftwareSeedSessionBinding::new(
                            &context.base_profile_uuid,
                            wallet_id,
                            session.binding().vault_session_id(),
                        );
                        let seed = session.open(grant, &binding)?;
                        let private_key =
                            derive_public_evm_private_key_from_seed(&seed, derivation_index)?;
                        let address = public_evm_address_from_private_key(&private_key)?;
                        if address != account.address {
                            return Err(VaultError::InvalidSoftwareContextIdentity);
                        }
                        Ok(private_key)
                    }
                    WalletSoftwareContextKind::Standard => {
                        let spend = grant.take_spend_unlock()?;
                        let spend_record =
                            self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
                        let spend_bundle = spend.decrypt_spend_bundle(wallet_id, &spend_record)?;
                        derive_public_evm_private_key_from_entropy(
                            &spend_bundle.bip39_entropy,
                            derivation_index,
                        )
                    }
                }
            }
            PublicAccountSource::Imported => {
                let spend = grant.take_spend_unlock()?;
                let record = self.encrypted_record(&public_account_secret_record_key(
                    &account.public_account_uuid,
                ))?;
                let secret =
                    spend.decrypt_public_account_secret(&account.public_account_uuid, &record)?;
                Ok(Zeroizing::new(secret.private_key))
            }
            // Executor keys may only be obtained by the live native owner while
            // its signing admission remains held through handoff.
            PublicAccountSource::HardwareDerived | PublicAccountSource::ExecutorDerived(_) => {
                Err(VaultError::InvalidPublicAccountOperation)
            }
        }
    }

    pub(in crate::vault) fn list_public_account_metadata_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(PUBLIC_ACCOUNT_METADATA_PREFIX)?;
        let mut accounts = Vec::with_capacity(records.len());
        for stored in records {
            let Some(public_account_uuid) = stored.key.strip_prefix(PUBLIC_ACCOUNT_METADATA_PREFIX)
            else {
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            let mut account = view.decrypt_public_account_metadata(public_account_uuid, &record)?;
            if account.public_account_uuid != public_account_uuid {
                public_account_uuid.clone_into(&mut account.public_account_uuid);
            }
            accounts.push(account);
        }
        sort_public_account_metadata(&mut accounts);
        Ok(accounts)
    }
}
