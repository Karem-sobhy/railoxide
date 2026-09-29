use std::time::Instant;

use alloy::primitives::{U256, address};
use alloy::providers::Provider as _;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};

use crate::settings::EffectiveChainGasSettings;

alloy::sol! {
    // Nitro's public price oracle. Alloy has no built-in binding for this precompile.
    // https://github.com/OffchainLabs/nitro-precompile-interfaces/blob/main/ArbGasInfo.sol
    interface ArbGasInfo {
        function getL1BaseFeeEstimate() external view returns (uint256);
    }
}

/// Price the additional hook calldata without exposing hooks, notes, or account addresses.
/// The hook-free `CoW` quote already covers the rest of the settlement's data cost.
pub(super) async fn hook_data_cost_from_rpc_pool(
    pool: &QueryRpcPool,
    chain_id: u64,
    app_data_len: usize,
    gas: &EffectiveChainGasSettings,
) -> Result<U256> {
    if chain_id != 42161 {
        return Ok(U256::ZERO);
    }
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "hook_data_cost", "started");
    let result = async {
        for _ in 0..pool.len() {
            let Some(handle) = pool.random_provider() else {
                break;
            };
            let response = handle
                .provider
                .call(
                    TransactionRequest::default()
                        .to(address!("000000000000000000000000000000000000006C"))
                        .input(ArbGasInfo::getL1BaseFeeEstimateCall {}.abi_encode().into()),
                )
                .await;
            if let Some(price) = response.ok().and_then(|bytes| {
                ArbGasInfo::getL1BaseFeeEstimateCall::abi_decode_returns(&bytes).ok()
            }) {
                // Hex in the app data uses two characters per hook byte. Keep the JSON
                // overhead too, plus 1 KiB for both trampoline/settlement ABI wrappers.
                // No compression discount: proofs and ciphertexts compress poorly.
                let bytes = U256::from(app_data_len.div_ceil(2)) + U256::from(1024);
                let cost = price
                    .checked_mul(U256::from(16))
                    .and_then(|price| price.checked_mul(bytes))
                    .and_then(|cost| cost.checked_mul(U256::from(gas.gas_price_buffer_numerator)))
                    .ok_or_else(|| eyre!("Arbitrum hook data cost overflow"))?;
                return Ok(cost.div_ceil(U256::from(gas.gas_price_buffer_denominator)));
            }
            tracing::warn!(rpc_index = handle.index, "fetch Arbitrum data price failed");
            pool.mark_bad_provider(&handle);
        }
        Err(eyre!("Could not estimate Arbitrum data costs. Try again."))
    }
    .await;
    tracing::debug!(
        target: "swap_quote",
        step = "hook_data_cost",
        elapsed_ms = started.elapsed().as_millis(),
        success = result.is_ok(),
        "finished"
    );
    result
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy::primitives::Bytes;
    use alloy::sol_types::SolValue as _;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn rollup_data_cost_reads_only_the_public_oracle_and_fails_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let http = crate::HttpContext::direct_for_tests();
        let pool =
            QueryRpcPool::with_http_client(vec![url], Duration::from_mins(1), http.rpc_client);
        let gas = EffectiveChainGasSettings {
            gas_limit_buffer: 0,
            gas_price_buffer_numerator: 3,
            gas_price_buffer_denominator: 2,
        };
        let serve = tokio::spawn(async move {
            // A valid oracle result, then an unavailable oracle. Other chains make no call.
            for result in [
                json!({"result": Bytes::from(U256::from(100).abi_encode())}),
                json!({"error": {"code": -32000, "message": "unavailable"}}),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let request = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n")
                        && let Ok(value) =
                            serde_json::from_slice::<serde_json::Value>(&bytes[end + 4..])
                    {
                        break value;
                    }
                };
                assert_eq!(request["method"], "eth_call");
                let call: TransactionRequest =
                    serde_json::from_value(request["params"][0].clone()).unwrap();
                assert_eq!(
                    call.to,
                    Some(address!("000000000000000000000000000000000000006C").into())
                );
                assert!(call.from.is_none());
                assert_eq!(
                    call.input.input().unwrap().as_ref(),
                    ArbGasInfo::getL1BaseFeeEstimateCall {}.abi_encode()
                );
                let mut response = result;
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                let body = response.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        for chain in [1, 56, 137] {
            assert_eq!(
                hook_data_cost_from_rpc_pool(&pool, chain, 8000, &gas)
                    .await
                    .unwrap(),
                U256::ZERO
            );
        }
        // 4,000 hook/metadata bytes + 1,024 ABI overhead; 100 wei per L1 gas,
        // 16 gas per byte, buffered by 3/2. No private payload is sent to the oracle.
        assert_eq!(
            hook_data_cost_from_rpc_pool(&pool, 42161, 8000, &gas)
                .await
                .unwrap(),
            U256::from(12_057_600)
        );
        assert!(
            hook_data_cost_from_rpc_pool(&pool, 42161, 8000, &gas)
                .await
                .is_err()
        );
        serve.await.unwrap();
    }
}
