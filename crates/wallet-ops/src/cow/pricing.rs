//! Hook gas estimate and order limit for a private swap.
//!
//! `CoW` quote verification doesn't count hook gas (design decision 5), so the wallet estimates
//! it here, prices the estimate in the buy token at the higher of `CoW`'s quoted gas price and
//! the buffered RPC price, and takes it off the quoted output together with slippage before
//! binding the private minimum. Solvers pay for the gas a hook uses, so the declared gas limit,
//! which only caps execution, is not priced. The estimate depends only on the chain and
//! transaction shapes. It reads no chain state and sends nothing to an RPC, so it works before
//! delegation and never exposes a signed hook.
//!
//! # Calibration
//!
//! Gas here is what a hook adds to a settlement: the hook call's own gas, the trampoline's call
//! overhead, and the hook calldata. It errs high on purpose. A high estimate only lowers the
//! guaranteed minimum, and the surplus still reaches the user. A low one makes the order
//! unattractive to solvers.
//!
//! The Railgun parts come from the shared per-chain model, [`RailgunGasModel`], in its
//! [`GasEstimateMode::UpperBound`] mode: the `RelayAdapt7702` execute
//! ([`RailgunGasModel::executor`]), the pre-hook's `transact`, and the post-hook's one-leaf
//! shield. Its docs hold the calibration and the per-chain samples. Only the calls a hook adds
//! around them are measured here, on anvil 1.7.1 mainnet forks at blocks 26,060,041, 26,060,057
//! and 26,060,078 with the executor delegated to `RelayAdapt7702` `0x05ae…d963`, running through
//! a copy of `HooksTrampoline` with fresh executors so every slot starts cold. They don't depend
//! on the chain's precompile pricing, so every chain uses them:
//!
//! - [`HOOK_PRE_CALLS_GAS`]: the deadline guard, the first exact approval, and the trampoline's
//!   call overhead (6,681 to 7,720 measured). An `execute` with no Railgun transactions measured
//!   90,940 including the executor call.
//! - [`INVALIDATE_ORDER_GAS`]: `GPv2Settlement.invalidateOrder`, measured 33,585.
//! - [`HOOK_GUARD_GAS`]: the post-hook's self-transfer guard and the trampoline's call overhead.
//!   Whole post-hooks measured 803,508 to 820,094 across tokens at the tree's position at the
//!   time, and 855,936 at leaf index 32,768.
//!
//! The first leaf of a fresh tree costs more than the upper bound, up to 902,453 for a post-hook
//! (`cbBTC` with Railgun and treasury balances starting at zero). That happens once per 65,536
//! leaves, so the declared limit's margin covers it rather than every order paying for it.

use alloy::primitives::{Address, U256, uint};
use broadcaster_core::contracts::shield::{ShieldFeeError, min_shield_amount};
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel, TransactionShape};

use super::CowQuoteParameters;
use crate::{FEE_BASIS_POINTS_DENOMINATOR, railgun_protocol_fee_amount};

/// The pre-hook's deadline guard, first exact approval, and trampoline call overhead.
const HOOK_PRE_CALLS_GAS: u64 = 40_000;
/// `GPv2Settlement.invalidateOrder` in the pre-hook. Measured 33,585 on a mainnet fork.
const INVALIDATE_ORDER_GAS: u64 = 35_000;
/// The post-hook's self-transfer guard and trampoline call overhead.
const HOOK_GUARD_GAS: u64 = 25_000;

const WEI_PER_NATIVE: U256 = uint!(1_000_000_000_000_000_000_U256);

/// Calls a pre-hook makes besides the deadline guard and the exact approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreHookCalls {
    /// Invalidates the executor's still-live earlier order (design decision 3).
    pub invalidate_order: bool,
}

/// Estimated gas of a pre-hook `execute` that runs `transactions` and `calls`.
#[must_use]
pub fn pre_hook_gas(
    model: &RailgunGasModel,
    transactions: &[TransactionShape],
    calls: PreHookCalls,
) -> u64 {
    let invalidate = if calls.invalidate_order {
        INVALIDATE_ORDER_GAS
    } else {
        0
    };
    model
        .executor()
        .saturating_add(HOOK_PRE_CALLS_GAS)
        .saturating_add(invalidate)
        .saturating_add(model.transact(GasEstimateMode::UpperBound, transactions.into()))
}

/// Estimated gas of the post-hook `multicall`: the guard and a full-balance shield of one leaf.
#[must_use]
pub const fn post_hook_gas(model: &RailgunGasModel) -> u64 {
    model
        .executor()
        .saturating_add(HOOK_GUARD_GAS)
        .saturating_add(model.shield(GasEstimateMode::UpperBound, 1))
}

/// Gas limit to declare for a hook in the app data: the estimate plus 10%. The margin covers
/// the 1/64 of gas a call keeps back from the calls it makes, and variation the measurements
/// didn't cover.
#[must_use]
pub const fn hook_gas_limit(estimate: u64) -> u64 {
    estimate.saturating_add(estimate / 10)
}

/// How to convert native gas cost into the buy token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeBuyRate {
    /// Buy-token base units per whole native token, from a fresh anchor's `buy_rate`.
    Anchor(U256),
    /// For a pair without anchors, whose unverified price the user has acknowledged. The rate
    /// comes from the quote: `CoW`'s `sellTokenPrice`, in wei per sell-token base unit, and the
    /// quoted buy-per-sell ratio. The wallet uses this price anyway, so it adds no new trust.
    Quote,
}

/// Inputs to [`price_order_limit`].
#[derive(Debug, Clone, Copy)]
pub struct OrderLimitParams<'a> {
    /// A hook-free sell quote. For an order with `feeAmount = 0` selling
    /// `sellAmount + feeAmount`, `buyAmount` is the expected output after `CoW`'s network and
    /// protocol fees.
    pub quote: &'a CowQuoteParameters,
    /// Sum of the pre-hook and post-hook gas estimates, without the declared limits' margin.
    pub hook_gas: u64,
    /// Buffered RPC gas price in wei. The quote's gas price is a floor, rounded up to whole wei.
    pub gas_price_wei: u128,
    /// Additional native cost for posting the hooks' calldata on rollups.
    pub hook_data_cost_wei: U256,
    pub native_rate: NativeBuyRate,
    pub slippage_bps: u32,
    pub shield_fee_bps: U256,
}

/// The order limit for a quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderLimit {
    /// Hook gas cost in buy-token base units.
    pub hook_cost: U256,
    /// Suggested minimum received privately, after the shield fee.
    pub min_received: U256,
    /// The order's `buyAmount` for `min_received`.
    pub buy_amount: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OrderLimitError {
    #[error("hook gas costs at least the quoted output")]
    HookCostExceedsOutput {
        buy_token: Address,
        /// Estimated hook cost in buy-token base units.
        hook_cost: U256,
        /// Quoted output before hook costs, in buy-token base units.
        quoted_output: U256,
    },
    #[error("nothing is left to receive after slippage and the shield fee")]
    NothingToReceive,
    #[error("slippage must be below 10000 basis points")]
    InvalidSlippage,
    #[error("the quote's native price can't be used")]
    InvalidQuotePrice,
    #[error("the quote's gas price can't be used")]
    InvalidQuoteGasPrice,
    #[error(transparent)]
    ShieldFee(#[from] ShieldFeeError),
    #[error("order amounts overflow")]
    Overflow,
}

/// Prices hook gas, slippage, and the shield fee into the order limit.
///
/// The suggested minimum is
/// `M = net((quoted - ceil(hook_cost)) * (10000 - slippage) / 10000)`, where `net` is Railgun's
/// inclusive shield fee deduction, `x - floor(x * fee / 10000)`. The order's `buyAmount` is
/// [`order_buy_amount`] of `M`. Every rounding lowers `M`: the hook cost and the quote-derived
/// rate round up, and slippage rounds down. `M` is thus never more than the quote supports, and
/// `buyAmount` never exceeds the amount delivered after slippage.
pub fn price_order_limit(params: &OrderLimitParams<'_>) -> Result<OrderLimit, OrderLimitError> {
    let slippage = U256::from(params.slippage_bps);
    if slippage >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(OrderLimitError::InvalidSlippage);
    }
    if params.shield_fee_bps >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(ShieldFeeError::FeeTooHigh.into());
    }
    let rate = match params.native_rate {
        NativeBuyRate::Anchor(rate) => rate,
        NativeBuyRate::Quote => quote_native_to_buy_rate(params.quote)?,
    };
    let cow_gas_price = quote_gas_price_wei(params.quote)?;
    let rpc_gas_price = U256::from(params.gas_price_wei);
    let gas_price = cow_gas_price.max(rpc_gas_price);
    tracing::debug!(
        target: "swap_quote",
        step = "gas_price_comparison",
        cow_gas_price_wei = %cow_gas_price,
        rpc_buffered_gas_price_wei = %rpc_gas_price,
        selected_gas_price_wei = %gas_price,
        cow_covers_rpc = cow_gas_price >= rpc_gas_price,
        "compared gas prices"
    );
    let hook_wei = U256::from(params.hook_gas)
        .checked_mul(gas_price)
        .and_then(|cost| cost.checked_add(params.hook_data_cost_wei))
        .ok_or(OrderLimitError::Overflow)?;
    let hook_cost = hook_wei
        .checked_mul(rate)
        .ok_or(OrderLimitError::Overflow)?
        .div_ceil(WEI_PER_NATIVE);
    let after_hooks = params
        .quote
        .buy_amount
        .checked_sub(hook_cost)
        .filter(|amount| !amount.is_zero())
        .ok_or(OrderLimitError::HookCostExceedsOutput {
            buy_token: params.quote.buy_token,
            hook_cost,
            quoted_output: params.quote.buy_amount,
        })?;
    let delivered = after_hooks
        .checked_mul(FEE_BASIS_POINTS_DENOMINATOR - slippage)
        .ok_or(OrderLimitError::Overflow)?
        / FEE_BASIS_POINTS_DENOMINATOR;
    let min_received = delivered - railgun_protocol_fee_amount(delivered, params.shield_fee_bps);
    Ok(OrderLimit {
        hook_cost,
        min_received,
        buy_amount: order_buy_amount(min_received, params.shield_fee_bps)?,
    })
}

/// `CoW` returns decimal wei and may use exponent notation. Alloy's `parse_units`
/// truncates fractions and doesn't read exponents, so reuse the quote-price decimal
/// adapter and round upward to avoid understating the solver's gas price.
fn quote_gas_price_wei(quote: &CowQuoteParameters) -> Result<U256, OrderLimitError> {
    let (mantissa, exponent) =
        parse_decimal(&quote.gas_price).ok_or(OrderLimitError::InvalidQuoteGasPrice)?;
    if mantissa.is_zero() {
        return Ok(U256::ZERO);
    }
    let scale = U256::from(10_u8).checked_pow(U256::from(exponent.unsigned_abs()));
    if exponent >= 0 {
        scale
            .and_then(|scale| mantissa.checked_mul(scale))
            .ok_or(OrderLimitError::InvalidQuoteGasPrice)
    } else {
        // If the divisor exceeds U256, this positive price is below one wei.
        Ok(scale.map_or(U256::ONE, |scale| mantissa.div_ceil(scale)))
    }
}

/// The order's `buyAmount` for a minimum received privately: the smallest amount whose net
/// after the shield fee is at least `min_received`.
pub fn order_buy_amount(min_received: U256, shield_fee_bps: U256) -> Result<U256, OrderLimitError> {
    min_shield_amount(min_received, shield_fee_bps).map_err(|error| match error {
        ShieldFeeError::ZeroMinimum => OrderLimitError::NothingToReceive,
        other => other.into(),
    })
}

/// Buy-token base units per whole native token implied by a quote, rounded up:
/// `1e18 * buyAmount / (sellTokenPrice * sellAmount)`.
fn quote_native_to_buy_rate(quote: &CowQuoteParameters) -> Result<U256, OrderLimitError> {
    let (price, exponent) =
        parse_decimal(&quote.sell_token_price).ok_or(OrderLimitError::InvalidQuotePrice)?;
    if price.is_zero() || quote.sell_amount.is_zero() {
        return Err(OrderLimitError::InvalidQuotePrice);
    }
    // 1e18 / (price * 10^exponent) = 10^(18 - exponent) / price.
    let scale = 18_i32
        .checked_sub(exponent)
        .ok_or(OrderLimitError::InvalidQuotePrice)?;
    let power = U256::from(10_u8)
        .checked_pow(U256::from(scale.unsigned_abs()))
        .ok_or(OrderLimitError::InvalidQuotePrice)?;
    let (numerator, denominator) = if scale >= 0 {
        (
            quote.buy_amount.checked_mul(power),
            price.checked_mul(quote.sell_amount),
        )
    } else {
        (
            Some(quote.buy_amount),
            price
                .checked_mul(quote.sell_amount)
                .and_then(|product| product.checked_mul(power)),
        )
    };
    let (Some(numerator), Some(denominator)) = (numerator, denominator) else {
        return Err(OrderLimitError::InvalidQuotePrice);
    };
    Ok(numerator.div_ceil(denominator))
}

/// Parses a non-negative decimal such as `412345678.9` or `4.1E-7` into
/// `(mantissa, exponent)` with value `mantissa * 10^exponent`.
fn parse_decimal(text: &str) -> Option<(U256, i32)> {
    let (number, exponent) = match text.split_once(['e', 'E']) {
        Some((number, exponent)) => (number, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = format!("{whole}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mantissa = U256::from_str_radix(&digits, 10).ok()?;
    let exponent = exponent.checked_sub(i32::try_from(fraction.len()).ok()?)?;
    Some((mantissa, exponent))
}

#[cfg(test)]
mod tests {
    use railgun_wallet::tx::{ETHEREUM_GAS_MODEL, POLYGON_GAS_MODEL};

    use super::*;

    const fn shape(
        input_count: usize,
        output_count: usize,
        has_unshield: bool,
    ) -> TransactionShape {
        TransactionShape {
            input_count,
            output_count,
            has_unshield,
        }
    }

    fn quote(sell_amount: u64, buy_amount: u64, sell_token_price: &str) -> CowQuoteParameters {
        CowQuoteParameters {
            sell_token: Address::ZERO,
            buy_token: Address::ZERO,
            receiver: None,
            sell_amount: U256::from(sell_amount),
            buy_amount: U256::from(buy_amount),
            valid_to: 0,
            fee_amount: U256::ZERO,
            gas_amount: "0".to_owned(),
            gas_price: "0".to_owned(),
            sell_token_price: sell_token_price.to_owned(),
            kind: "sell".to_owned(),
            partially_fillable: false,
        }
    }

    /// Measured gas from the fork runs in the module docs. Fork rows are
    /// `(transactions of (inputs, outputs), invalidates order, measured gas)`, all with an
    /// unshield to the executor.
    #[test]
    fn estimate_covers_measured_hook_gas() {
        type ForkRow = (&'static [(usize, usize)], bool, u64);
        let fork: &[ForkRow] = &[
            (&[], false, 90_940),
            (&[], true, 124_525),
            (&[(1, 1)], false, 534_326),
            (&[(1, 2)], false, 1_173_858),
            (&[(1, 2)], false, 1_233_418),
            (&[(1, 2)], false, 1_200_969),
            (&[(1, 2)], true, 1_159_565),
            (&[(12, 2)], false, 1_653_804),
            (&[(13, 1)], false, 1_001_860),
            (&[(1, 2), (1, 2)], false, 2_029_134),
            (&[(12, 2), (1, 2)], false, 2_511_882),
            (&[(12, 2), (1, 2), (1, 1)], true, 2_952_758),
            (&[(12, 2), (12, 2)], true, 2_851_133),
            (&[(12, 2), (12, 2), (12, 2)], true, 3_592_500),
            (&[(1, 2); 8], true, 4_460_207),
            (&[(12, 2); 8], true, 7_441_501),
            (&[(13, 1); 8], false, 6_252_944),
        ];
        for (transactions, invalidate_order, measured) in fork {
            let shapes: Vec<_> = transactions
                .iter()
                .map(|(inputs, outputs)| shape(*inputs, *outputs, true))
                .collect();
            let calls = PreHookCalls {
                invalidate_order: *invalidate_order,
            };
            assert!(
                pre_hook_gas(&ETHEREUM_GAS_MODEL, &shapes, calls) >= *measured,
                "{transactions:?} {invalidate_order}"
            );
        }
        // Post-hooks at the tree's position at the time, and at leaf index 32,768.
        for measured in [803_508, 820_094, 855_936] {
            assert!(post_hook_gas(&ETHEREUM_GAS_MODEL) >= measured);
        }
        // The first leaf of a fresh tree is not priced, but the declared limit must let it run.
        for measured in [902_453, 885_487, 878_423] {
            assert!(hook_gas_limit(post_hook_gas(&ETHEREUM_GAS_MODEL)) >= measured);
        }
        // The heaviest Polygon `RelayAdapt7702` shield-only call.
        assert!(post_hook_gas(&POLYGON_GAS_MODEL) >= 936_417);
    }

    #[test]
    fn order_limit_prices_hook_gas_slippage_and_shield_fee() {
        // 1,000 USDC quoted; 2M gas at 1 gwei costs 6 USDC at 3,000 USDC/native.
        // A rollup's additional 0.001 native data cost must also reduce the order limit.
        let quote = quote(1, 1_000_000_000, "1");
        for (data_cost, total_cost, received) in [
            (0_u64, 6_000_000_u64, 986_557_425_u64),
            (1_000_000_000_000_000, 9_000_000, 983_579_888),
        ] {
            let limit = price_order_limit(&OrderLimitParams {
                quote: &quote,
                hook_gas: 2_000_000,
                gas_price_wei: 1_000_000_000,
                hook_data_cost_wei: U256::from(data_cost),
                native_rate: NativeBuyRate::Anchor(U256::from(3_000_000_000_u64)),
                slippage_bps: 50,
                shield_fee_bps: U256::from(25),
            })
            .unwrap();
            assert_eq!(limit.hook_cost, U256::from(total_cost));
            assert_eq!(limit.min_received, U256::from(received));
            assert_eq!(
                limit.buy_amount,
                min_shield_amount(limit.min_received, U256::from(25)).unwrap()
            );
        }
    }

    #[test]
    fn order_limit_covers_cow_gas_when_rpc_is_cheaper() {
        // The expired 50 USDC -> DAI order's quote, scaled after its 0.431971 USDC
        // base fee. Its hooks declare 2,479,400 gas, including their 10% margin.
        let mut quote = quote(49_568_029, 1, "371805436.07432949542999267578125");
        quote.buy_amount = uint!(49_567_285_817_433_747_187_U256);
        quote.gas_price = "843216447".into();
        let params = OrderLimitParams {
            quote: &quote,
            hook_gas: 2_479_400,
            gas_price_wei: 90_505_453,
            hook_data_cost_wei: U256::ZERO,
            native_rate: NativeBuyRate::Quote,
            slippage_bps: 50,
            shield_fee_bps: U256::from(25),
        };
        let limit = price_order_limit(&params).unwrap();
        assert_eq!(limit.hook_cost, uint!(5_622_939_607_751_008_887_U256));
        assert_eq!(limit.min_received, uint!(43_615_312_917_437_738_797_U256));

        // A higher buffered RPC price must still take precedence.
        let higher = price_order_limit(&OrderLimitParams {
            gas_price_wei: 1_000_000_000,
            ..params
        })
        .unwrap();
        assert!(higher.hook_cost > limit.hook_cost);
        assert!(higher.min_received < limit.min_received);
    }

    #[test]
    fn rounding_never_overstates_the_minimum() {
        // 1 wei of gas cost rounds up to one base unit; 10,001 * 0.9999 rounds down to 9,999.
        let quote = quote(1, 10_002, "1");
        let limit = price_order_limit(&OrderLimitParams {
            quote: &quote,
            hook_gas: 1,
            gas_price_wei: 1,
            hook_data_cost_wei: U256::ZERO,
            native_rate: NativeBuyRate::Anchor(U256::ONE),
            slippage_bps: 1,
            shield_fee_bps: U256::from(25),
        })
        .unwrap();
        assert_eq!(limit.hook_cost, U256::ONE);
        assert_eq!(limit.min_received, U256::from(9_975));
        assert_eq!(limit.buy_amount, U256::from(9_999));

        // A quote-derived rate of 1e18 / 3 rounds up.
        let quote = self::quote(1, 1, "3");
        assert_eq!(
            quote_native_to_buy_rate(&quote),
            Ok(U256::from(333_333_333_333_333_334_u64))
        );

        // CoW gas prices are decimal wei, including exponent notation. A fraction
        // must round upward before pricing hooks, even when the RPC price is lower.
        let mut quote = self::quote(1, 100, "1");
        for gas_price in ["1.01", "1.01e0", "0.101E+1"] {
            quote.gas_price = gas_price.into();
            let limit = price_order_limit(&OrderLimitParams {
                quote: &quote,
                hook_gas: 1,
                gas_price_wei: 1,
                hook_data_cost_wei: U256::ZERO,
                native_rate: NativeBuyRate::Anchor(WEI_PER_NATIVE),
                slippage_bps: 0,
                shield_fee_bps: U256::ZERO,
            })
            .unwrap();
            assert_eq!(limit.hook_cost, U256::from(2), "{gas_price}");
        }
    }

    #[test]
    fn unusable_quote_gas_does_not_fall_back_to_a_cheaper_rpc_price() {
        let mut quote = quote(1, 100, "1");
        for gas_price in ["", "-1", "NaN", "1e100"] {
            quote.gas_price = gas_price.into();
            assert!(
                price_order_limit(&OrderLimitParams {
                    quote: &quote,
                    hook_gas: 1,
                    gas_price_wei: 1,
                    hook_data_cost_wei: U256::ZERO,
                    native_rate: NativeBuyRate::Anchor(WEI_PER_NATIVE),
                    slippage_bps: 0,
                    shield_fee_bps: U256::ZERO,
                })
                .is_err(),
                "{gas_price}"
            );
        }
    }

    #[test]
    fn quote_rate_reads_plain_and_exponent_prices() {
        // 1 USDC buys 0.00025 WETH, and CoW prices a USDC base unit at 2.5e8 wei.
        for price in ["250000000", "250000000.0", "2.5E+8", "2.5e8"] {
            let quote = quote(1_000_000, 250_000_000_000_000, price);
            assert_eq!(
                quote_native_to_buy_rate(&quote),
                Ok(WEI_PER_NATIVE),
                "{price}"
            );
        }
        for price in ["", ".", "-1", "1e", "NaN", "0"] {
            assert_eq!(
                quote_native_to_buy_rate(&quote(1, 1, price)),
                Err(OrderLimitError::InvalidQuotePrice),
                "{price}"
            );
        }
    }

    #[test]
    fn hook_cost_at_or_above_the_output_is_rejected() {
        let mut quote = quote(1, 6_000_000, "1");
        quote.buy_token = Address::repeat_byte(2);
        let params = OrderLimitParams {
            quote: &quote,
            hook_gas: 2_000_000,
            gas_price_wei: 1_000_000_000,
            hook_data_cost_wei: U256::ZERO,
            native_rate: NativeBuyRate::Anchor(U256::from(3_000_000_000_u64)),
            slippage_bps: 0,
            shield_fee_bps: U256::from(25),
        };
        for hook_gas in [2_000_000, 3_000_000] {
            assert_eq!(
                price_order_limit(&OrderLimitParams { hook_gas, ..params }),
                Err(OrderLimitError::HookCostExceedsOutput {
                    buy_token: quote.buy_token,
                    hook_cost: U256::from(hook_gas * 3),
                    quoted_output: quote.buy_amount,
                })
            );
        }
    }
}
