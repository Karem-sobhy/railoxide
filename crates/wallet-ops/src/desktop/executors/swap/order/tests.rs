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

#[test]
fn swap_planning_offers_the_largest_amount_that_fits_one_order() {
    let profile =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap()
            .swap_profile()
            .unwrap();
    let builder = TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    };
    let delegated = DelegatedSwapExecutor {
        operation: ExecutorOperationId::random().unwrap(),
        executor: Address::repeat_byte(0xe0),
        delegate: Address::repeat_byte(0xde),
        setup_payload: B256::repeat_byte(3),
        observed: ExecutorNonceObservation::new(
            BlockNumHash::new(12, B256::repeat_byte(12)),
            U256::ONE,
        ),
    };
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
