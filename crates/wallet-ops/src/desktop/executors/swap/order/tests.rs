use alloy::eips::BlockNumHash;
use alloy::primitives::address;
use railgun_wallet::{Note, UtxoCommitmentKind, UtxoSource};

use super::*;
use crate::vault::ExecutorNonceObservation;

const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");

fn note(tree: u32, position: u64, value: u64) -> Utxo {
    let mut random = [0; 16];
    random[..4].copy_from_slice(&tree.to_be_bytes());
    random[8..].copy_from_slice(&position.to_be_bytes());
    Utxo::new(
        Note::new_change(U256::ONE, WETH, U256::from(value), random),
        tree,
        position,
        UtxoSource {
            tx_hash: B256::repeat_byte(6),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    )
}

fn mainnet_profile() -> SwapProfile {
    crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .swap_profile()
        .unwrap()
}

fn builder() -> TransactionBuilder {
    TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    }
}

fn delegated_executor() -> DelegatedSwapExecutor {
    DelegatedSwapExecutor {
        operation: ExecutorOperationId::random().unwrap(),
        executor: Address::repeat_byte(0xe0),
        delegate: Address::repeat_byte(0xde),
        setup_payload: B256::repeat_byte(3),
        observed: ExecutorNonceObservation::new(
            BlockNumHash::new(12, B256::repeat_byte(12)),
            U256::ONE,
        ),
    }
}

#[test]
fn swap_planning_offers_the_largest_amount_that_fits_one_order() {
    let profile = mainnet_profile();
    let builder = builder();
    let delegated = delegated_executor();
    let budget = profile.app_data_byte_budget();
    let plan = |utxos: &[Utxo], amount: U256| {
        plan_swap_inputs(
            &builder,
            &profile,
            delegated,
            utxos,
            &SwapAmountRequest {
                sell_token: WETH,
                buy_token: USDC,
                amount,
                delivery: SwapDelivery::Reshield,
                byte_budget: None,
            },
            budget,
            None,
        )
        .unwrap()
    };

    // Too many notes: sixty notes of one tree need five transactions.
    let many = (0..60)
        .map(|position| note(0, position, 1))
        .collect::<Vec<_>>();
    // Spread across trees: each of nine trees needs its own transaction, one more than a batch.
    let spread = (0..9).map(|tree| note(tree, 0, 10)).collect::<Vec<_>>();
    for (utxos, entered) in [(&many, 60_u64), (&spread, 90)] {
        let SwapAmountPlan::TooLarge { largest } = plan(utxos.as_slice(), U256::from(entered))
        else {
            panic!("{entered} does not fit one order");
        };
        assert!(largest.amount() < U256::from(entered));
        assert!(largest.app_data_len() <= budget);
        // The offer is a plan in its own right, the only kind a pre-hook proof is built from.
        assert_eq!(
            plan(utxos.as_slice(), largest.amount()),
            SwapAmountPlan::Fits(largest)
        );
    }
}

// An External order pays its receiver directly: no post-hook in its gas, limits or app data,
// no shield fee in its buy amount, and its quote still names only the executor.
#[test]
fn external_delivery_prices_only_the_pre_hook_and_no_shield_fee() {
    let profile = mainnet_profile();
    let delegated = delegated_executor();
    let receiver = Address::repeat_byte(0x99);
    let notes = [note(0, 0, 1_000_000)];
    let plan = |buy_token, delivery| {
        let SwapAmountPlan::Fits(plan) = plan_swap_inputs(
            &builder(),
            &profile,
            delegated,
            &notes,
            &SwapAmountRequest {
                sell_token: WETH,
                buy_token,
                amount: U256::from(1_000_000),
                delivery,
                byte_budget: None,
            },
            profile.app_data_byte_budget(),
            None,
        )
        .unwrap() else {
            panic!("one note fits one order");
        };
        plan
    };
    let reshield = plan(USDC, SwapDelivery::Reshield);
    let external = plan(Address::ZERO, SwapDelivery::External { receiver });
    assert_eq!(external.post_hook_gas_limit(), None);
    assert_eq!(
        external.hook_gas_estimate(),
        reshield.hook_gas_estimate() - post_hook_gas(reshield.gas_model)
    );
    assert!(external.app_data_len() < reshield.app_data_len());

    // 10,002 quoted less a 1-unit hook cost and 1 bp slippage delivers 9,999.
    let quote: CowQuote = serde_json::from_value(serde_json::json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC,
            "sellAmount": "997500", "buyAmount": "10002",
            "validTo": 1, "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let price = |plan: &SwapInputPlan| {
        price_swap_review(
            plan.clone(),
            quote.clone(),
            SwapPrice::Verified {
                rate: PairAnchorRate {
                    sell_rate: U256::ONE,
                    buy_rate: U256::ONE,
                },
                observations: Vec::new(),
            },
            U256::from(25),
            U256::from(25),
            1,
            1,
            U256::ZERO,
            OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        )
        .unwrap()
    };
    let (reshield_review, external_review) = (price(&reshield), price(&external));
    assert_eq!(external_review.shield_fee_bps(), U256::ZERO);
    assert_eq!(
        external_review.suggested_private_minimum(),
        U256::from(9_999)
    );
    assert_eq!(
        reshield_review.suggested_private_minimum(),
        U256::from(9_975)
    );
    // An approved minimum of 9,975 is the External buy amount; Reshield grosses it up.
    let minimum = U256::from(9_975);
    assert_eq!(external_review.buy_amount_for(minimum).unwrap(), minimum);
    assert_eq!(
        reshield_review.buy_amount_for(minimum).unwrap(),
        U256::from(9_999)
    );

    let request = swap_quote_request(&external, U256::from(997_500), 1);
    assert_eq!(
        (request.from, request.receiver, request.buy_token),
        (delegated.executor, delegated.executor, BUY_NATIVE_TOKEN)
    );
    // The native asset is anchored by the chain's wrapped-native token.
    assert_eq!(anchor_token(1, Address::ZERO), WETH);
}

#[test]
fn orderbook_app_data_rejection_returns_to_a_smaller_offer() {
    let uid = OrderUid::new(B256::repeat_byte(1), Address::repeat_byte(2), 3);
    assert_eq!(
        swap_submission_outcome(Err(CowApiError::AppDataTooLarge), uid, 14_000, 14_336).unwrap(),
        SwapOrderOutcome::Replan {
            byte_budget: 13_999,
            attempt_recorded: true,
        }
    );
    assert!(swap_submission_outcome(Err(CowApiError::NoLiquidity), uid, 14_000, 14_336).is_err());
}

#[test]
fn cached_swap_prices_cannot_silently_drop_or_bypass_verification() {
    let rate = PairAnchorRate {
        sell_rate: U256::from(997_500),
        buy_rate: U256::from(2_000_000),
    };
    let quote: CowQuote = serde_json::from_value(serde_json::json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC,
            "sellAmount": "997500", "buyAmount": "2000000",
            "validTo": 1, "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let verified = SwapPrice::Verified {
        rate,
        observations: Vec::new(),
    };
    assert!(matches!(
        recheck_swap_price(&quote.quote, &verified, None, 300).unwrap(),
        SwapRecheck::Changed(SwapReviewChange::PriceUnavailable)
    ));
    assert!(matches!(
        recheck_swap_price(&quote.quote, &SwapPrice::Unverified, None, 300).unwrap(),
        SwapRecheck::Current(observations) if observations.is_empty()
    ));
    let cached = |buy_rate| Some(PairAnchorRate { buy_rate, ..rate });
    assert!(matches!(
        recheck_swap_price(
            &quote.quote,
            &SwapPrice::Unverified,
            cached(rate.buy_rate),
            300
        )
        .unwrap(),
        SwapRecheck::Current(_)
    ));
    assert!(matches!(
        recheck_swap_price(
            &quote.quote,
            &SwapPrice::Unverified,
            cached(rate.buy_rate * U256::from(2)),
            300
        )
        .unwrap(),
        SwapRecheck::Changed(SwapReviewChange::QuoteDeviates)
    ));
}
