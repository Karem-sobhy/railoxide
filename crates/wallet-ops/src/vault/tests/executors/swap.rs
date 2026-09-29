use super::*;
use alloy::primitives::FixedBytes;
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
        post_hook_gas_limit: Some(300_000),
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
        post_hook: Some(hook(
            ExecutorPayloadPurpose::SwapPostHook,
            post_nonce,
            hooks + 1,
            delegate,
            observed,
            Vec::new(),
        )),
    };

    // A post-hook is admitted only alongside its pre-hook, at exactly k + 1.
    let first = attempt(30, 1_000, 31, 2);
    assert!(matches!(
        store.record_issued(operation, first.post_hook.clone().unwrap()),
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
        (retry.pre_hook().nonce(), retry.post_hook().unwrap().nonce()),
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
    after_completion.post_hook = Some(hook(
        ExecutorPayloadPurpose::SwapPostHook,
        4,
        42,
        delegate,
        after_fill,
        Vec::new(),
    ));
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

#[test]
fn swap_records_written_before_external_delivery_decode_as_reshield() {
    // Match the named MessagePack swap record before External delivery: a required post-hook
    // and post-hook gas limit, and a setup approval without its delivery or token pair.
    #[derive(serde::Serialize)]
    enum EarlierDelivery {
        Reshield,
    }
    #[derive(serde::Serialize)]
    struct EarlierBounds {
        sell_amount: U256,
        unshield_amount: Option<U256>,
        unshield_fee_bps: U256,
        buy_amount: U256,
        private_minimum: U256,
        shield_fee_bps: U256,
        slippage_bps: u32,
        pre_hook_gas_limit: u64,
        post_hook_gas_limit: u64,
        hook_cost: Option<U256>,
        anchors: Vec<SwapAnchorObservation>,
    }
    #[derive(serde::Serialize)]
    struct EarlierHook {
        nonce: U256,
        payload: B256,
    }
    #[derive(serde::Serialize)]
    struct EarlierOrder {
        terms: Option<SwapTerms>,
        attempt: u32,
        uid: FixedBytes<56>,
        delivery: EarlierDelivery,
        bounds: EarlierBounds,
        pre_hook: EarlierHook,
        post_hook: EarlierHook,
        invalidates: Option<FixedBytes<56>>,
        observations: SwapOrderObservations,
        submission: Option<SwapSubmission>,
        submission_status: SwapSubmissionStatus,
    }
    #[derive(serde::Serialize)]
    struct EarlierSwap {
        terms: SwapTerms,
        proof: SwapProof,
        orders: Vec<EarlierOrder>,
    }
    #[derive(serde::Serialize)]
    struct EarlierApproval {
        bounds: EarlierBounds,
        price_verified: Option<bool>,
        price_acknowledged: bool,
    }
    #[derive(serde::Serialize)]
    struct SavedRecord<Approval> {
        version: u32,
        derivation: ExecutorDerivationScheme,
        origin: ExecutorRecordOrigin,
        operation: ExecutorOperationId,
        index: u32,
        address: Address,
        delegate: Address,
        retired: bool,
        assets: Vec<ExecutorAsset>,
        issued: Vec<IssuedExecutorPayload>,
        swap: Option<EarlierSwap>,
        swap_approval: Approval,
    }
    fn saved(
        assets: Vec<ExecutorAsset>,
        swap: Option<EarlierSwap>,
        swap_approval: impl serde::Serialize,
    ) -> ExecutorRecord {
        let saved = SavedRecord {
            version: 1,
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            origin: ExecutorRecordOrigin::Reserved,
            operation: ExecutorOperationId::random().unwrap(),
            index: 7,
            address: Address::repeat_byte(2),
            delegate: Address::repeat_byte(1),
            retired: false,
            assets,
            issued: Vec::new(),
            swap,
            swap_approval,
        };
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&saved).unwrap()).unwrap()
    }
    let bounds = || EarlierBounds {
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
        anchors: Vec::new(),
    };
    let (sell, buy) = (Address::repeat_byte(5), Address::repeat_byte(6));
    let terms = SwapTerms::new(
        sell,
        buy,
        SwapRecipient::new(U256::from(7), [8; 32]),
        B256::repeat_byte(3),
    );
    let reshield = saved(
        vec![ExecutorAsset::Erc20(sell), ExecutorAsset::Erc20(buy)],
        Some(EarlierSwap {
            terms,
            proof: SwapProof::new(B256::repeat_byte(20), Vec::new()),
            orders: vec![EarlierOrder {
                terms: Some(terms),
                attempt: 0,
                uid: FixedBytes::repeat_byte(30),
                delivery: EarlierDelivery::Reshield,
                bounds: bounds(),
                pre_hook: EarlierHook {
                    nonce: U256::ONE,
                    payload: B256::repeat_byte(31),
                },
                post_hook: EarlierHook {
                    nonce: U256::from(2),
                    payload: B256::repeat_byte(32),
                },
                invalidates: None,
                observations: SwapOrderObservations::default(),
                submission: None,
                submission_status: SwapSubmissionStatus::Accepted,
            }],
        }),
        EarlierApproval {
            bounds: bounds(),
            price_verified: Some(true),
            price_acknowledged: false,
        },
    );
    let order = &reshield.swap().unwrap().orders()[0];
    assert_eq!(order.delivery(), SwapDelivery::Reshield);
    assert_eq!(
        order.post_hook().map(|hook| (hook.nonce(), hook.payload())),
        Some((U256::from(2), B256::repeat_byte(32)))
    );
    assert_eq!(order.bounds().post_hook_gas_limit, Some(300_000));
    let approval = reshield.swap_approval().unwrap();
    assert_eq!(
        (
            approval.delivery,
            approval.tokens,
            approval.bounds.post_hook_gas_limit
        ),
        (SwapDelivery::Reshield, None, Some(300_000))
    );
    // The earlier approval's pair is the setup's assets, in sell-then-buy order.
    assert_eq!(reshield.swap_approval_tokens(), Some((sell, buy)));

    // An External approval keeps its own pair, including a native Buy the assets never hold.
    let external = SwapApproval {
        delivery: SwapDelivery::External {
            receiver: Address::repeat_byte(9),
        },
        tokens: Some(SwapApprovalTokens {
            sell,
            buy: Address::ZERO,
        }),
        ..approval.clone()
    };
    let native_buy = saved(vec![ExecutorAsset::Erc20(sell)], None, &external);
    assert_eq!(native_buy.swap_approval(), Some(&external));
    assert_eq!(
        native_buy.swap_approval_tokens(),
        Some((sell, Address::ZERO))
    );
    for record in [reshield, native_buy] {
        assert_eq!(
            rmp_serde::from_slice::<ExecutorRecord>(&rmp_serde::to_vec_named(&record).unwrap())
                .unwrap(),
            record
        );
    }
}

#[test]
fn external_swap_attempts_issue_only_their_pre_hook_and_their_trade_admits_the_next() {
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
    let sell = Address::repeat_byte(5);
    store
        .reserve(operation, delegate, None, &[ExecutorAsset::Erc20(sell)])
        .unwrap();
    store.bind_address(operation, executor).unwrap();
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, before_setup, &[]).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            hook(
                ExecutorPayloadPurpose::Operation,
                0,
                3,
                delegate,
                before_setup,
                Vec::new(),
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
    let receiver = Address::repeat_byte(9);
    let external = SwapDelivery::External { receiver };
    let attempt = |delivery: SwapDelivery, post_hook: Option<IssuedExecutorPayload>| SwapAttempt {
        submission: None,
        // A native Buy asset, paid to the receiver directly.
        terms: SwapTerms::new(
            sell,
            Address::ZERO,
            SwapRecipient::new(U256::from(7), [8; 32]),
            setup,
        ),
        proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
        uid: OrderUid::new(B256::repeat_byte(30), executor, 1_000),
        delivery,
        bounds: SwapApprovedBounds {
            sell_amount: U256::from(9_975),
            unshield_amount: Some(U256::from(10_000)),
            unshield_fee_bps: U256::from(25),
            buy_amount: U256::from(9_975),
            private_minimum: U256::from(9_975),
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 900_000,
            post_hook_gas_limit: None,
            hook_cost: Some(U256::ZERO),
            anchors: Vec::new(),
        },
        invalidates: None,
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            1,
            31,
            delegate,
            observed,
            inputs.clone(),
        ),
        post_hook,
    };

    // The post-hook's presence must match the delivery kind.
    let post_hook = hook(
        ExecutorPayloadPurpose::SwapPostHook,
        2,
        32,
        delegate,
        observed,
        Vec::new(),
    );
    for mismatched in [
        attempt(external, Some(post_hook)),
        attempt(SwapDelivery::Reshield, None),
    ] {
        assert!(matches!(
            store.record_swap_attempt(operation, mismatched),
            Err(ExecutorStoreError::OperationMismatch)
        ));
    }

    let recorded = store
        .record_swap_attempt(operation, attempt(external, None))
        .unwrap();
    let [setup_payload, pre_hook] = recorded.issued() else {
        panic!("an External attempt issues only its pre-hook");
    };
    assert_eq!(setup_payload.hash(), setup);
    assert_eq!(
        (pre_hook.purpose(), pre_hook.nonce()),
        (ExecutorPayloadPurpose::SwapPreHook, U256::ONE)
    );
    let order = &recorded.swap().unwrap().orders()[0];
    assert_eq!(order.delivery(), external);
    assert!(order.post_hook().is_none());
    assert!(!recorded.records_future_nonce(observed.nonce()));
    // The native marker is never recorded as an ERC-20 asset.
    assert_eq!(recorded.assets(), &[ExecutorAsset::Erc20(sell)]);

    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(restored, recorded);

    // The verified trade alone delivers an External order, which never carries a credit.
    let traded = SwapObservation {
        block: BlockNumHash::new(13, B256::repeat_byte(13)),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    let amounts = SwapTradeAmounts {
        sell_amount: U256::from(9_975),
        buy_amount: U256::from(9_975),
        fee_amount: U256::ZERO,
    };
    let credit = SwapShieldObservation {
        observation: traded,
        private_amount: U256::from(9_975),
        fee: None,
    };
    assert!(matches!(
        store.record_swap_settlement(operation, order.uid(), traded, amounts, Some(credit)),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    let settled = store
        .record_swap_settlement(operation, order.uid(), traded, amounts, None)
        .unwrap();
    let observations = settled.swap().unwrap().orders()[0].observations();
    assert_eq!(
        (observations.traded, observations.delivered),
        (Some(traded), Some(traded))
    );
    assert!(settled.swap().unwrap().admits_attempt());

    // Reuse at finalized depth needs only the fresh nonce k + 1, where the next attempt signs.
    let after_trade =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(2));
    assert!(settled.settled_swaps_at(after_trade.block().number));
    store
        .refresh_settled_swap_nonce(&settled, after_trade)
        .unwrap();
    let mut next = attempt(external, None);
    next.uid = OrderUid::new(B256::repeat_byte(40), executor, 2_000);
    next.pre_hook = hook(
        ExecutorPayloadPurpose::SwapPreHook,
        2,
        41,
        delegate,
        after_trade,
        inputs.clone(),
    );
    let recorded = store.record_swap_attempt(operation, next).unwrap();
    assert_eq!(
        recorded.swap().unwrap().orders()[1].pre_hook().nonce(),
        U256::from(2)
    );
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
