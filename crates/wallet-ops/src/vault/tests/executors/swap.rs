use super::*;
use broadcaster_core::contracts::cow::OrderUid;

fn hook(
    purpose: ExecutorPayloadPurpose,
    nonce: u64,
    hash: u8,
    delegate: Address,
    observed: ExecutorNonceObservation,
    inputs: Vec<ExecutorInputIdentity>,
) -> IssuedExecutorPayload {
    IssuedExecutorPayload::new(
        U256::from(nonce),
        delegate,
        B256::repeat_byte(hash),
        purpose,
        ExecutorPayloadContext::new(Bytes::from_static(b"signed hook"), observed, inputs),
    )
}

#[test]
fn swap_hooks_persist_with_their_order_and_a_retry_waits_for_a_dead_pre_hook() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    let executor = Address::repeat_byte(2);
    store.reserve(operation, delegate, None, &[]).unwrap();
    store.bind_address(operation, executor).unwrap();

    // The delegation-only setup wins nonce 0, so the swap's pre-hook uses k = 1.
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, before_setup, &[]).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), before_setup, Vec::new()),
            ),
        )
        .unwrap();
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.reconcile(operation, observed, &[setup_won]).unwrap();

    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    let terms = SwapTerms::new(
        Address::repeat_byte(5),
        Address::repeat_byte(6),
        SwapRecipient::new(U256::from(7), [8; 32]),
        setup,
    );
    let bounds = SwapApprovedBounds {
        sell_amount: U256::from(9_975),
        unshield_amount: Some(U256::from(10_000)),
        unshield_fee_bps: U256::from(25),
        buy_amount: U256::from(9_999),
        private_minimum: U256::from(9_975),
        shield_fee_bps: U256::from(25),
        slippage_bps: 50,
        pre_hook_gas_limit: 900_000,
        post_hook_gas_limit: 300_000,
        hook_cost: Some(U256::ZERO),
        anchors: vec![SwapAnchorObservation {
            source: Address::repeat_byte(10),
            block: observed.block(),
            block_timestamp: 1_700_000_000,
            updated_at: Some(1_699_990_000),
        }],
    };
    let attempt = |digest: u8, valid_to: u32, hooks: u8, post_nonce: u64| SwapAttempt {
        submission: None,
        terms,
        proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
        uid: OrderUid::new(B256::repeat_byte(digest), executor, valid_to),
        delivery: SwapDelivery::Reshield,
        bounds: bounds.clone(),
        invalidates: None,
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            1,
            hooks,
            delegate,
            observed,
            inputs.clone(),
        ),
        post_hook: hook(
            ExecutorPayloadPurpose::SwapPostHook,
            post_nonce,
            hooks + 1,
            delegate,
            observed,
            Vec::new(),
        ),
    };

    // A post-hook is admitted only alongside its pre-hook, at exactly k + 1.
    let first = attempt(30, 1_000, 31, 2);
    assert!(matches!(
        store.record_issued(operation, first.post_hook.clone()),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.record_swap_attempt(operation, attempt(30, 1_000, 31, 3)),
        Err(ExecutorStoreError::OutstandingNonce)
    ));
    store.record_swap_attempt(operation, first.clone()).unwrap();

    // The pre-hook reserves its inputs against every other operation.
    let other = ExecutorOperationId::random().unwrap();
    store.reserve(other, delegate, None, &[]).unwrap();
    store.bind_address(other, Address::repeat_byte(40)).unwrap();
    store.reconcile(other, observed, &[]).unwrap();
    assert!(matches!(
        store.record_issued(
            other,
            hook(
                ExecutorPayloadPurpose::Operation,
                1,
                41,
                delegate,
                observed,
                inputs.clone()
            )
        ),
        Err(ExecutorStoreError::InputReserved)
    ));

    // The exception does not extend to other payloads: an ordinary operation
    // still cannot follow a pre-hook whose outcome is unknown.
    let consumed =
        ExecutorNonceObservation::new(BlockNumHash::new(13, B256::repeat_byte(13)), U256::from(2));
    // The spent pre-hook nonce resolves the pre-hook, but the post-hook can still run.
    assert!(
        store
            .reconcile(operation, consumed, &[setup_won])
            .unwrap()
            .has_unresolved_issued_work()
    );
    assert!(matches!(
        store.record_issued(
            operation,
            hook(
                ExecutorPayloadPurpose::Operation,
                2,
                42,
                delegate,
                consumed,
                Vec::new()
            )
        ),
        Err(ExecutorStoreError::OutstandingNonce)
    ));

    // After expiry the nonce is still k, and a retry waits for recorded death.
    store.reconcile(operation, observed, &[setup_won]).unwrap();
    assert!(matches!(
        store.record_swap_attempt(operation, attempt(32, 2_000, 33, 2)),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    // A rejection doesn't prove the signed pre-hook can't run, even for a retry that names it.
    store
        .record_swap_submission(operation, first.uid, SwapSubmissionStatus::Rejected)
        .unwrap();
    let mut invalidating = attempt(32, 2_000, 33, 2);
    invalidating.invalidates = Some(first.uid);
    assert!(matches!(
        store.record_swap_attempt(operation, invalidating),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    let dead = SwapOrderObservations {
        pre_hook_dead: Some(SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(operation, first.uid, dead)
        .unwrap();
    let mut different_pair = attempt(32, 2_000, 33, 2);
    let new_terms = SwapTerms::new(
        Address::repeat_byte(7),
        Address::repeat_byte(8),
        terms.recipient(),
        setup,
    );
    different_pair.terms = new_terms;
    let recorded = store
        .record_swap_attempt(operation, different_pair)
        .unwrap();

    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(restored, recorded);
    let swap = restored.swap().unwrap();
    assert_eq!(swap.terms(), &new_terms);
    let [expired, retry] = swap.orders() else {
        panic!("one order per attempt");
    };
    assert_eq!(swap.order_terms(expired), &terms);
    assert_eq!(swap.order_terms(retry), &new_terms);
    assert_eq!(expired.observations(), dead);
    assert_eq!((retry.attempt(), retry.valid_to()), (1, 2_000));
    assert_eq!(retry.bounds(), &bounds);
    assert_eq!(
        (retry.pre_hook().nonce(), retry.post_hook().nonce()),
        (U256::ONE, U256::from(2))
    );
    assert_eq!(restored.issued().len(), 5);
    assert_eq!(restored.reserved_inputs(), inputs);
    // An old order lacking per-order terms keeps the original pair after later reuse. An old
    // order without the unshield split sold its whole unshield amount. Observations from
    // before trade amounts were kept still decode.
    let mut legacy = serde_json::to_value(&restored).unwrap();
    let legacy_order = legacy["swap"]["orders"][0].as_object_mut().unwrap();
    legacy_order.remove("terms");
    legacy_order["observations"]
        .as_object_mut()
        .unwrap()
        .remove("trade_amounts")
        .unwrap();
    let legacy_bounds = legacy_order["bounds"].as_object_mut().unwrap();
    legacy_bounds.remove("unshield_amount");
    legacy_bounds.remove("unshield_fee_bps");
    let legacy: ExecutorRecord = serde_json::from_value(legacy).unwrap();
    let legacy_swap = legacy.swap().unwrap();
    assert_eq!(legacy_swap.order_terms(&legacy_swap.orders()[0]), &terms);
    assert_eq!(legacy_swap.orders()[0].observations(), dead);
    assert_eq!(legacy_swap.terms(), &new_terms);
    let legacy_bounds = legacy_swap.orders()[0].bounds();
    assert_eq!(
        (legacy_bounds.spend_amount(), legacy_bounds.unshield_fee_bps),
        (bounds.sell_amount, U256::ZERO)
    );
    // A shield observed before its fee was recorded decodes with the fee unknown.
    let shield = SwapShieldObservation {
        observation: SwapObservation {
            block: observed.block(),
            transaction_hash: Some(B256::repeat_byte(49)),
        },
        private_amount: bounds.private_minimum,
        fee: Some(U256::from(25u8)),
    };
    let mut legacy_shield = serde_json::to_value(shield).unwrap();
    legacy_shield
        .as_object_mut()
        .unwrap()
        .remove("fee")
        .unwrap();
    assert_eq!(
        serde_json::from_value::<SwapShieldObservation>(legacy_shield).unwrap(),
        SwapShieldObservation {
            fee: None,
            ..shield
        }
    );

    // Completed settlement also permits reuse, including its consumed post-hook nonce.
    let after_fill =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(3));
    store
        .reconcile(operation, after_fill, &[setup_won])
        .unwrap();
    let delivered = SwapObservation {
        block: after_fill.block(),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    // Hooks never get a direct-call status; nonces past every hook resolve them.
    assert!(
        !store
            .record_swap_observations(
                operation,
                retry.uid(),
                SwapOrderObservations {
                    pre_hook_executed: Some(delivered),
                    traded: Some(delivered),
                    delivered: Some(delivered),
                    shielded: Some(SwapShieldObservation {
                        observation: delivered,
                        private_amount: bounds.private_minimum,
                        fee: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap()
            .has_unresolved_issued_work()
    );
    // Without a fresh nonce, recorded outcomes can't resolve a hook.
    assert!(
        store
            .invalidate_observation(operation)
            .unwrap()
            .has_recorded_unresolved_issued_work()
    );
    store
        .reconcile(operation, after_fill, &[setup_won])
        .unwrap();
    let mut after_completion = attempt(40, 3_000, 41, 4);
    after_completion.terms = new_terms;
    after_completion.pre_hook = hook(
        ExecutorPayloadPurpose::SwapPreHook,
        3,
        41,
        delegate,
        after_fill,
        inputs.clone(),
    );
    after_completion.post_hook = hook(
        ExecutorPayloadPurpose::SwapPostHook,
        4,
        42,
        delegate,
        after_fill,
        Vec::new(),
    );
    assert_eq!(
        store
            .record_swap_attempt(operation, after_completion)
            .unwrap()
            .swap()
            .unwrap()
            .orders()
            .len(),
        3
    );
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
