use super::super::*;
use super::helpers::*;

// --- Pure page/range helpers (no vault needed) ---

#[test]
fn derived_page_one_covers_indexes_zero_through_nineteen() {
    let indexes = derived_address_page_indexes(1).expect("page 1 indexes");
    assert_eq!(indexes.len(), DERIVED_ADDRESS_BROWSE_PAGE_SIZE as usize);
    assert_eq!(indexes.len(), 20);
    assert_eq!(indexes[0], 0);
    assert_eq!(indexes[19], 19);
}

#[test]
fn derived_pages_advance_by_twenty() {
    let page_two = derived_address_page_indexes(2).expect("page 2 indexes");
    assert_eq!(page_two.len(), 20);
    assert_eq!(page_two[0], 20);
    assert_eq!(page_two[19], 39);

    let page_three = derived_address_page_indexes(3).expect("page 3 indexes");
    assert_eq!(page_three.len(), 20);
    assert_eq!(page_three[0], 40);
    assert_eq!(page_three[19], 59);
}

#[test]
fn derived_page_start_index_is_deterministic() {
    assert_eq!(derived_address_page_start_index(1), Some(0));
    assert_eq!(derived_address_page_start_index(2), Some(20));
    assert_eq!(derived_address_page_start_index(12), Some(220));
    assert_eq!(derived_address_page_start_index(0), None);
}

#[test]
fn derived_page_zero_is_invalid() {
    assert!(matches!(
        validate_derived_address_page(0),
        Err(VaultError::InvalidDerivedAddressPage)
    ));
    assert!(derived_address_page_indexes(0).is_none());
}

#[test]
fn derived_range_generates_start_through_start_plus_count() {
    let indexes = validate_derived_address_range(100, 5).expect("range 100..105");
    assert_eq!(indexes, vec![100, 101, 102, 103, 104]);
}

#[test]
fn derived_range_rejects_empty_and_oversized_counts() {
    assert!(matches!(
        validate_derived_address_range(0, 0),
        Err(VaultError::InvalidDerivedAddressRange)
    ));
    assert!(matches!(
        validate_derived_address_range(0, MAX_DERIVED_ADDRESS_BATCH_COUNT + 1),
        Err(VaultError::DerivedAddressBatchTooLarge(_))
    ));
    assert!(validate_derived_address_range(0, MAX_DERIVED_ADDRESS_BATCH_COUNT).is_ok());
}

#[test]
fn derived_range_rejects_overflow_past_index_ceiling() {
    assert!(matches!(
        validate_derived_address_range(MAX_DERIVED_ADDRESS_INDEX, 2),
        Err(VaultError::InvalidDerivedAddressRange)
    ));
    // A single index at the ceiling is fine.
    assert_eq!(
        validate_derived_address_range(MAX_DERIVED_ADDRESS_INDEX, 1).expect("ceiling index"),
        vec![MAX_DERIVED_ADDRESS_INDEX]
    );
}

// --- Vault-backed preview and batch-add behavior ---

fn preview_page(
    store: &DesktopVaultStore,
    session: &DesktopViewSession,
    page: u32,
) -> Vec<DerivedAddressPreview> {
    store
        .preview_derived_public_address_page(TEST_PASSWORD, session, page, None)
        .expect("preview page")
}

#[test]
fn preview_page_matches_sequential_derivation() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-preview-wallet", "Batch");
    let page = preview_page(&store, &view_session, 1);
    assert_eq!(page.len(), 20);
    for (offset, preview) in page.iter().enumerate() {
        let expected = derive_public_evm_address_from_mnemonic_with_passphrase(
            TEST_MNEMONIC,
            "",
            offset as u32,
        )
        .expect("expected address");
        assert_eq!(preview.derivation_index, offset as u32);
        assert_eq!(preview.address, expected);
    }
    drop(store);
    drop(db);
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn preview_uses_current_wallet_key_not_another_key() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first = import_wallet_with_metadata(&store, "batch-wallet-a", "A");
    let second = import_wallet_with_metadata(&store, "batch-wallet-b", "B");
    // Both wallets share the test mnemonic here, but the sessions are distinct;
    // previewing through each session must succeed independently.
    assert_eq!(preview_page(&store, &first, 2).len(), 20);
    assert_eq!(preview_page(&store, &second, 2).len(), 20);
    drop(store);
    drop(db);
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn preview_rejects_wrong_password() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-bad-password", "Batch");
    assert!(
        store
            .preview_derived_public_addresses(TEST_PASSWORD, &view_session, &[0], None)
            .is_ok()
    );
    assert!(
        store
            .preview_derived_public_addresses("wrong password", &view_session, &[0], None)
            .is_err()
    );
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}

#[test]
fn preview_rejects_duplicate_and_empty_index_requests() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-dup-request", "Batch");
    assert!(
        store
            .preview_derived_public_addresses(TEST_PASSWORD, &view_session, &[1, 1], None)
            .is_err()
    );
    assert!(
        store
            .preview_derived_public_addresses(TEST_PASSWORD, &view_session, &[], None)
            .is_err()
    );
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}

#[test]
fn batch_add_inserts_range_and_reports_summary() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-add-wallet", "Batch");
    // Index 0 already exists from wallet creation.
    let outcome = store
        .add_derived_public_accounts_in_range(TEST_PASSWORD, &view_session, 0, 5, None)
        .expect("batch add 0..5");
    assert_eq!(outcome.added.len(), 4);
    assert_eq!(outcome.skipped_indexes, vec![0]);
    assert_eq!(
        outcome.summary(),
        "5 addresses processed — 4 added, 1 already existed."
    );
    let added_indexes: Vec<u32> = outcome
        .added
        .iter()
        .filter_map(|account| account.derivation_index)
        .collect();
    assert_eq!(added_indexes, vec![1, 2, 3, 4]);
    // Every inserted account carries an EVM address derived from the wallet key.
    for account in &outcome.added {
        let expected = derive_public_evm_address_from_mnemonic_with_passphrase(
            TEST_MNEMONIC,
            "",
            account.derivation_index.expect("derivation index"),
        )
        .expect("expected address");
        assert_eq!(account.address, expected);
    }
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}

#[test]
fn batch_add_is_idempotent_and_never_duplicates() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-idempotent", "Batch");
    let first = store
        .add_derived_public_accounts_in_range(TEST_PASSWORD, &view_session, 0, 5, None)
        .expect("first batch");
    assert_eq!(first.added.len(), 4);
    let second = store
        .add_derived_public_accounts_in_range(TEST_PASSWORD, &view_session, 0, 5, None)
        .expect("second batch");
    assert!(second.added.is_empty());
    assert_eq!(second.skipped_indexes, vec![0, 1, 2, 3, 4]);
    // Re-adding a subset by explicit index is also fully skipped.
    let third = store
        .add_derived_public_accounts_at_indexes(TEST_PASSWORD, &view_session, &[2, 4], None)
        .expect("repeat indexes");
    assert!(third.added.is_empty());
    assert_eq!(third.skipped_indexes, vec![2, 4]);
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}

#[test]
fn batch_add_skips_imported_address_duplicates() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-import-dup", "Batch");
    // Import the private key for derived index 3, then batch over it.
    let derived_key =
        derive_public_evm_private_key_from_mnemonic(TEST_MNEMONIC, 3).expect("derived key");
    store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            &format!("0x{}", alloy::hex::encode(derived_key)),
            Some("Imported index 3"),
            false,
        )
        .expect("import duplicate address");
    let outcome = store
        .add_derived_public_accounts_in_range(TEST_PASSWORD, &view_session, 1, 5, None)
        .expect("batch over imported duplicate");
    let added: Vec<u32> = outcome
        .added
        .iter()
        .filter_map(|account| account.derivation_index)
        .collect();
    assert_eq!(added, vec![1, 2, 4, 5]);
    assert_eq!(outcome.skipped_indexes, vec![3]);
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}

#[test]
fn batch_add_rejects_invalid_ranges() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-invalid", "Batch");
    assert!(matches!(
        store.add_derived_public_accounts_in_range(TEST_PASSWORD, &view_session, 0, 0, None),
        Err(VaultError::InvalidDerivedAddressRange)
    ));
    assert!(matches!(
        store.add_derived_public_accounts_in_range(
            TEST_PASSWORD,
            &view_session,
            0,
            MAX_DERIVED_ADDRESS_BATCH_COUNT + 1,
            None
        ),
        Err(VaultError::InvalidDerivedAddressRange | VaultError::DerivedAddressBatchTooLarge(_))
    ));
    assert!(
        store
            .add_derived_public_accounts_at_indexes(TEST_PASSWORD, &view_session, &[], None)
            .is_err()
    );
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}

#[test]
fn batch_add_counts_inactive_derivation_index_as_existing() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "batch-inactive", "Batch");
    let added = store
        .add_derived_public_account(TEST_PASSWORD, &view_session, Some("Derived 1"))
        .expect("single derived");
    assert_eq!(added.derivation_index, Some(1));
    store
        .deactivate_derived_public_account(&view_session, &added.public_account_uuid)
        .expect("deactivate");
    // Index 1 exists but is inactive: the batch must skip it, not duplicate it.
    let outcome = store
        .add_derived_public_accounts_in_range(TEST_PASSWORD, &view_session, 0, 3, None)
        .expect("batch over inactive");
    let added_indexes: Vec<u32> = outcome
        .added
        .iter()
        .filter_map(|account| account.derivation_index)
        .collect();
    assert_eq!(added_indexes, vec![2]);
    assert_eq!(outcome.skipped_indexes, vec![0, 1]);
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
    drop(db);
}
