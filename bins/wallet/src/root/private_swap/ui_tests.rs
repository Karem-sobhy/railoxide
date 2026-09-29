use super::super::*;
use super::*;
use crate::root::chain_load::{ChainUtxoState, WalletSyncLifecycle};
use gpui::{IntoElement, ParentElement, Render, Styled, TestAppContext, div};
use gpui_component::{Root, WindowExt};
use std::cell::Cell;
use std::rc::Rc;
use wallet_ops::vault::ExecutorStore;

/// Mount the root as an entity, as startup does. Calling its render helper through
/// `read` would miss attempts to read the root while GPUI holds its update lease.
struct WalletWindow(Entity<WalletRoot>);

impl Render for WalletWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone()).children(
            crate::root::startup::render_wallet_overlay_layers(window, cx),
        )
    }
}

#[gpui::test]
fn swap_cost_confirmation_wraps_and_is_cleared_when_terms_change(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
            });
            window.close_all_dialogs(cx);
            let swaps = swaps.clone();
            // Mount the production warning with a high-cost percentage. Quote creation is
            // exercised by wallet-ops tests; this checks the control and its consent lifetime.
            window.open_dialog(cx, move |dialog, _, cx| {
                let warning = swaps.update(cx, |swaps, cx| {
                    PrivateSwapsView::render_cost_acknowledgement(
                        swaps.form.as_ref().unwrap(),
                        Some(1_234),
                        true,
                        cx,
                    )
                });
                dialog.w(gpui::px(360.)).children(warning)
            });
            window.set_rem_size(gpui::px(22.));
            window.draw(cx).clear(cx);
        });
        let alert = cx.debug_bounds("swap-high-costs").unwrap();
        let message = cx.debug_bounds("swap-high-cost-message").unwrap();
        let checkbox = cx.debug_bounds("swap-costs-acknowledged").unwrap();
        assert!(alert.size.width <= gpui::px(360.));
        assert!(
            checkbox.top() >= message.bottom(),
            "confirmation follows the full warning"
        );
        assert!(
            checkbox.bottom() <= alert.bottom()
                && checkbox.left() >= alert.left()
                && checkbox.right() <= alert.right(),
            "confirmation stays inside the alert"
        );
        assert!(!swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().high_costs_acknowledged
        }));
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        assert!(swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().high_costs_acknowledged
        }));
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_slippage(100, window, cx);
                assert!(
                    !swaps.form.as_ref().unwrap().high_costs_acknowledged,
                    "consent to the previous quote cannot carry across changed terms"
                );
            });
        });
    });
}

#[gpui::test]
fn swap_max_starts_a_quote_for_the_exact_available_amount(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    let available = U256::from(305_133);
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
            });
            swaps.update(cx, |swaps, cx| {
                swaps.open_form(None, usdc, Some(dai), None, None, window, cx);
                assert!(matches!(
                    swaps.form.as_ref().unwrap().quote,
                    QuoteState::Idle
                ));
            });
            window.close_all_dialogs(cx);
            let swaps = swaps.clone();
            // Feed the real Sell panel a spendable balance without starting private sync.
            window.open_dialog(cx, move |dialog, _, cx| {
                let panel = swaps.update(cx, |swaps, cx| {
                    swaps.render_sell_panel(
                        swaps.form.as_ref().unwrap(),
                        &[UnshieldAsset {
                            chain_id: 1,
                            token: usdc,
                            label: "USDC".into(),
                            decimals: Some(6),
                            total: available,
                            poi_verified_total: available,
                            max_batched: available,
                            icon_path: None,
                        }],
                        true,
                        false,
                        cx,
                    )
                });
                dialog.child(panel)
            });
            window.draw(cx).clear(cx);
        });
        let max_button = cx.debug_bounds("swap-amount-max").unwrap();
        cx.simulate_click(max_button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(swaps.form_amount(form, cx).unwrap(), available);
            assert!(
                matches!(form.quote, QuoteState::Loading),
                "Max starts quoting"
            );
            assert!(form.quote_task.is_some());
        });
    });
}

#[gpui::test]
fn buy_asset_picker_searches_and_keeps_selection_in_sync(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
            });
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(usdc, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let trigger = cx.debug_bounds("swap-buy-selector").unwrap();
        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("DAI");
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            assert_eq!(
                swaps.form.as_ref().unwrap().buy_select.read(cx).query(cx),
                "DAI"
            );
        });
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.buy, Some(dai));
                assert_eq!(form.buy_select.read(cx).selected_value(), Some(dai));

                swaps.flip_tokens(window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (dai, Some(usdc)));
                assert_eq!(form.buy_select.read(cx).selected_value(), Some(usdc));

                swaps.set_form_sell(usdc, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.buy.is_none());
                assert!(form.buy_select.read(cx).selected_value().is_none());
            });
            window.draw(cx).clear(cx);
        });
    });
}

#[gpui::test]
fn swap_quote_error_wraps_within_its_column(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
            });
        });
        for error in [
            eyre::eyre!(
                "The CoW orderbook quote request to https://api.cow.fi/ failed: the connection closed before a response was received. Check your connection and try again."
            ),
            eyre::Report::from(wallet_ops::cow::OrderLimitError::HookCostExceedsOutput {
                buy_token: Address::repeat_byte(2),
                hook_cost: U256::from(60_000_000),
                quoted_output: U256::from(49_500_000),
            })
            .wrap_err("Could not price the order"),
        ] {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let revision = swaps.form.as_ref().unwrap().quote_revision;
                    swaps.apply_quote(
                        None,
                        revision,
                        Some(QuoteResult {
                            orderbook: None,
                            outcome: Err(error),
                        }),
                        window,
                        cx,
                    );
                });
            });
            for (width, font_size) in [(700., 16.), (600., 22.)] {
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(1100.)));
                cx.update(|window, cx| {
                    window.set_rem_size(gpui::px(font_size));
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
                let status = cx.debug_bounds("swap-price-status").unwrap();
                let error = cx.debug_bounds("swap-price-error").unwrap();
                let retry = cx.debug_bounds("swap-price-retry").unwrap();
                assert!(
                    error.right() <= status.right(),
                    "the error must stay out of the balance column: {error:?}, {status:?}"
                );
                assert!(
                    retry.top() >= error.bottom(),
                    "Retry follows the full error"
                );
                assert!(retry.bottom() <= status.bottom());
            }
        }
    });
}

#[gpui::test]
fn quote_retry_discards_the_old_route_and_ignores_its_late_response(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, operation, runtime, cx| {
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(&swaps.owner));
        let old_client = runtime.block_on(owner.swap_orderbook_client()).unwrap();
        for operation in [None, Some(operation)] {
            let mut old_revision = 0;
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_form(
                        operation,
                        Address::repeat_byte(1),
                        Some(Address::repeat_byte(2)),
                        Some(U256::ONE),
                        None,
                        window,
                        cx,
                    );
                    old_revision = swaps.form.as_ref().unwrap().quote_revision;
                    swaps.apply_quote(
                        operation,
                        old_revision,
                        Some(QuoteResult {
                            orderbook: Some(old_client.clone()),
                            outcome: Err(eyre::eyre!("connection failed")),
                        }),
                        window,
                        cx,
                    );
                });
                window.draw(cx).clear(cx);
            });
            let retry = cx.debug_bounds("swap-price-retry").unwrap();
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // A superseded response cannot put the failed route back into either
                    // cache, including the operation cache used when resuming a swap.
                    swaps.apply_quote(
                        operation,
                        old_revision,
                        Some(QuoteResult {
                            orderbook: Some(old_client.clone()),
                            outcome: Err(eyre::eyre!("late connection failure")),
                        }),
                        window,
                        cx,
                    );
                    let form = swaps.form.as_ref().unwrap();
                    assert!(matches!(form.quote, QuoteState::Loading));
                    assert!(form.orderbook.is_none(), "Retry must request a fresh route");
                    if let Some(operation) = operation {
                        assert!(
                            swaps.tracking[&operation].orderbook.is_none(),
                            "Retry must not fall back to the tracked route"
                        );
                    }
                });
            });
        }
    });
}

#[gpui::test]
fn private_swap_progress_preserves_other_dialogs_and_can_restart_a_retired_setup(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let rendered = Rc::new(Cell::new(false));
        cx.update(|window, cx| {
            let rendered = rendered.clone();
            window.open_dialog(cx, move |dialog, _, _| {
                rendered.set(true);
                dialog.title("Unrelated form").child("Keep this form open")
            });
            window.draw(cx).clear(cx);
        });
        rendered.set(false);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.finish_order(
                    operation,
                    wallet_ops::SwapOrderOutcome::Replan {
                        byte_budget: 1_000,
                        attempt_recorded: true,
                    },
                    window,
                    cx,
                );
            });
            window.refresh();
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
        });
        assert!(
            rendered.get(),
            "the unrelated dialog still renders after the swap completes"
        );
        // A preparation may retire an account after detecting prior chain activity. A retry
        // must open a fresh swap review instead of resubmitting that unusable reservation.
        executors.retire(operation).unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let stage = swaps.stage(swaps.record(operation).unwrap());
                assert_eq!(stage, model::SwapStage::SetupRetired);
                let actions = model::swap_actions(stage, false);
                assert!(actions.resume.is_some_and(|available| available.is_ok()));
                // Ended, it leaves the Private tab by itself.
                assert!(actions.recover && !actions.dismiss);
                swaps.open_existing_form(operation, window, cx);
                assert!(swaps.form.as_ref().unwrap().operation().is_none());
                assert!(swaps.record(operation).unwrap().is_retired());
            });
            window.draw(cx).clear(cx);
        });
    });
}

#[gpui::test]
fn selected_account_swap_allows_changing_both_tokens_without_starting_setup(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadInclusion,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let setup = pending_setup(executors, operation);
        // The setup won its nonce, so a new swap can offer this account.
        executors
            .reconcile(
                operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(12, B256::repeat_byte(12)),
                    U256::ONE,
                ),
                &[(
                    setup.issued()[0].hash(),
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        cx.update(|window, cx| {
            let target = crate::root::stealth_accounts::StealthAccountTarget::new(
                &swaps.read(cx).session,
                operation,
            );
            root.update(cx, |root, cx| {
                root.open_stealth_account(&target, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let menu = cx
            .debug_bounds(format!("stealth-row-menu-{}", operation.opaque_id()).leak())
            .unwrap();
        cx.simulate_click(menu.center(), gpui::Modifiers::none());
        // The menu starts with Add to Public, then Use for swap.
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps
                    .form
                    .as_ref()
                    .expect("the selected account opens a swap form");
                assert_eq!(form.operation, Some(operation));
                assert!(form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Order);
                assert!(swaps.job.is_none());
                let select = form.account_select.as_ref().unwrap();
                assert_eq!(select.read(cx).selected_value(), Some(&Some(operation)));
                let sell = alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
                let buy = alloy::primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
                swaps.set_form_sell(sell, window, cx);
                swaps.set_form_buy(buy, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (sell, Some(buy)));
                assert_eq!(form.operation, Some(operation));
            });
            window.draw(cx).clear(cx);
            window.close_all_dialogs(cx);
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.operation.is_none() && !form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Setup { resume: false });
                // A normally opened swap offers the set-up account.
                let select = form.account_select.clone().unwrap();
                select.update(cx, |select, cx| {
                    select.set_selected_value(&Some(operation), window, cx);
                    assert_eq!(select.selected_value(), Some(&Some(operation)));
                    cx.emit(
                        SelectEvent::<SearchableVec<SwapAccountSelectItem>>::Confirm(Some(Some(
                            operation,
                        ))),
                    );
                });
            });
            window.draw(cx).clear(cx);
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert!(form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Order);
                assert!(swaps.job.is_none());
                // Back to a new account: the swap is no longer bound to the chosen one.
                swaps.select_form_account(None, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.operation.is_none() && !form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Setup { resume: false });
                assert_eq!(
                    form.account_select
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .selected_value(),
                    Some(&None)
                );
            });
        });
    });
}

/// A submitted setup whose broadcaster has not confirmed it. Its fee input must
/// survive a retry, stop, and restart because the signed payload can still execute.
fn pending_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
) -> wallet_ops::vault::ExecutorRecord {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload,
    };
    let record = executors
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    executors
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    executors.reconcile(operation, observed, &[]).unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16197, "commitment": "0x1"
    }))
    .unwrap();
    executors
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                record.delegate(),
                B256::repeat_byte(4),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), observed, vec![input]),
            ),
        )
        .unwrap()
}

#[gpui::test]
fn setup_confirmation_tracks_local_inclusion_without_advancing_the_swap(cx: &mut TestAppContext) {
    use crate::root::utxo::UtxoFinalityContext;
    use alloy::primitives::B256;
    with_swap_view(cx, |_, _, executors, operation, _, _| {
        let record = pending_setup(executors, operation);
        let transaction = B256::repeat_byte(80);
        let record = executors
            .record_submission(operation, record.issued()[0].hash(), transaction)
            .unwrap();
        let mut utxo = wallet_ops::UtxoOutput {
            tree: 0,
            position: 0,
            token: String::new(),
            value: "1".into(),
            commitment_kind: "Transact".into(),
            activity_classification: "Private Output".into(),
            blocked_shield_rescue: None,
            commitment: String::new(),
            npk: String::new(),
            blinded_commitment: String::new(),
            poi_statuses: BTreeMap::new(),
            ppoi_state: wallet_ops::UtxoPpoiState::Unknown,
            ppoi_last_submission_at: None,
            poi_spendable: false,
            source_tx_hash: B256::repeat_byte(90).to_string(),
            source_block_number: 90,
            source_block_timestamp: 0,
            is_spent: false,
            pending_new: false,
            pending_spent: true,
            local_pending_spent: false,
            spent_tx_hash: Some(transaction.to_string()),
            spent_block_number: Some(100),
        };
        // The spend is already visible in private sync, but not yet safe.
        for (head, expected) in [
            (100, "Confirming (0/12 blocks)"),
            (105, "Confirming (5/12 blocks)"),
            (112, "Verifying setup…"),
        ] {
            assert_eq!(
                model::swap_setup_confirmation(&record, std::slice::from_ref(&utxo))
                    .and_then(|confirmation| confirmation.detail(UtxoFinalityContext::new(
                        Some(head),
                        Some(head - 12),
                        Some(12),
                    )))
                    .as_deref(),
                Some(expected)
            );
            assert_eq!(swap_stage(&record, None, false), SwapStage::SetupPending);
        }
        // If private sync rolls the inclusion back, an unrelated transaction must
        // not leave the setup showing a stale confirmation count.
        utxo.spent_tx_hash = None;
        utxo.spent_block_number = None;
        let finality = UtxoFinalityContext::new(Some(105), Some(93), Some(12));
        assert!(model::swap_setup_confirmation(&record, std::slice::from_ref(&utxo)).is_none());
        // Received change can provide the same hint even if the spent note is absent.
        utxo.source_tx_hash = transaction.to_string();
        utxo.source_block_number = 100;
        assert_eq!(
            model::swap_setup_confirmation(&record, &[utxo])
                .and_then(|confirmation| confirmation.detail(finality))
                .as_deref(),
            Some("Confirming (5/12 blocks)")
        );
    });
}

#[gpui::test]
fn observation_catchup_continues_successful_pages_but_waits_after_failure(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(400);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let confirmed = swaps.confirmed_block(cx).unwrap();
                let result = |end, outcome| {
                    vec![ObservationResult {
                        operation,
                        range_end: end,
                        outcome,
                    }]
                };
                assert_eq!(
                    swaps.apply_observations(result(164, Ok(None)), window, cx),
                    [operation],
                    "a successful page behind the safe head continues without the polling delay"
                );
                let (_, _, pages) = swaps.next_observations(cx).unwrap();
                assert_eq!(pages[0].range, 164..228);
                assert!(
                    swaps
                        .apply_observations(result(228, Err("RPC unavailable".into())), window, cx,)
                        .is_empty(),
                    "failed reads wait before retrying"
                );
                assert_eq!(
                    swaps.tracking.get(&operation).unwrap().cursor,
                    Some(164),
                    "a failed page must not skip unobserved blocks"
                );
                assert!(
                    swaps
                        .apply_observations(result(confirmed + 1, Ok(None)), window, cx,)
                        .is_empty(),
                    "caught-up reads return to the polling interval"
                );
            });
        });
    });
}

#[gpui::test]
fn pending_setup_can_retry_and_stop_without_losing_its_reservation(cx: &mut TestAppContext) {
    use alloy::eips::{BlockNumHash, eip7702::constants::EIP7702_DELEGATION_DESIGNATOR};
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadInclusion,
    };
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let issued = pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, test_approval())
            .unwrap();
        // Hiding an account is only presentation; it must not stop its pending swap.
        executors.set_hidden(operation, true).unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        // A setup unconfirmed for long is checked every few minutes, not every block.
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(1_000);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(observed(swaps, cx));
                swaps.tracking.entry(operation).or_default().setup_read_at = Some(Instant::now());
                assert!(!observed(swaps, cx));
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at =
                    Instant::now().checked_sub(DEFERRED_SETUP_OBSERVATION_INTERVAL);
                assert!(observed(swaps, cx));
            });
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(swaps.has_shown_swaps());
                assert!(!swaps.record(operation).unwrap().is_swap_setup_stopped());
                swaps.tracking.entry(operation).or_default().auto_place = true;
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::SetupPending
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let retry = cx.debug_bounds("swap-progress-continue").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert_eq!(swaps.form_mode(form), FormMode::Setup { resume: true });
                assert!(!swaps.tracking.get(&operation).unwrap().auto_place);
                let preview = swaps.owner.swap_setup_preview(operation).unwrap();
                assert_eq!(preview.executor(), issued.address().unwrap());
                assert!(preview.delegated().is_none());
                swaps.form = None;
                // A local delivery may still be waiting when the user stops it.
                let join = runtime.spawn(std::future::pending::<()>());
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: join.abort_handle(),
                });
                swaps.tracking.get_mut(&operation).unwrap().auto_place = true;
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let stop = cx.debug_bounds("swap-progress-stop").unwrap();
        cx.simulate_click(stop.center(), gpui::Modifiers::none());
        // Confirm the alert through its normal keyboard action.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|_, cx| {
            let swaps = swaps.read(cx);
            let record = swaps.record(operation).unwrap();
            assert!(record.is_swap_setup_stopped());
            assert!(!swaps.has_shown_swaps());
            assert!(swaps.job.is_none());
            assert!(!swaps.tracking.get(&operation).unwrap().auto_place);
            assert_eq!(record.reserved_inputs(), issued.reserved_inputs());
        });
        // Nothing waits on a stopped setup, so the swap view stops reading its account.
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = None;
                assert!(!observed(swaps, cx));
            });
        });
        // Unhiding the account cannot undo the persisted stop, even if setup confirms later.
        executors.set_hidden(operation, false).unwrap();
        assert!(
            executors
                .records()
                .unwrap()
                .iter()
                .find(|record| record.operation() == operation)
                .unwrap()
                .is_swap_setup_stopped()
        );
        assert!(
            executors
                .record_swap_approval(operation, test_approval())
                .is_err()
        );
        let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
        let record = executors
            .reconcile(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::ONE),
                &[(
                    issued.issued()[0].hash(),
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        let profile = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
        });
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        let setup = wallet_ops::swap_setup_status(&record, confirmed, &code, profile);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.setup = Some(setup);
                tracking.auto_place = true;
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::Approved
                );
                assert!(!swaps.has_shown_swaps());
                swaps.continue_approved_swaps(window, cx);
                assert!(swaps.pending_authorization.is_none());
                assert!(swaps.job.is_none());
            });
        });
    });
}

#[gpui::test]
fn handed_off_setup_waits_for_its_located_inclusion(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadInclusion, ExecutorPayloadPurpose, ExecutorPayloadStatus,
        IssuedExecutorPayload,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let issued = pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, test_approval())
            .unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        // Whether a reload counted a newly recorded setup inclusion since the last check.
        let seen = std::cell::Cell::new(0);
        let woke = |swaps: &PrivateSwapsView| {
            let wakes = *swaps.observation_wake.borrow();
            seen.replace(wakes) != wakes
        };
        let history_start = issued.issued()[0].context().history_start();
        let set_head = |head, cx: &mut gpui::App| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(head);
            });
        };
        let depth = cx.update(|_, cx| {
            root.read(cx)
                .effective_chain_configs
                .get(1)
                .unwrap()
                .finality_depth
        });
        // The confirmed block is one short of the setup's history start plus the depth.
        let early_head = history_start + 2 * depth - 1;
        cx.update(|_, cx| {
            set_head(early_head, cx);
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(
                    !woke(swaps),
                    "a setup without an inclusion does not wake polling"
                );
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.setup = Some(wallet_ops::SwapSetupStatus::Pending);
                tracking.setup_read_at = Some(Instant::now());
                assert!(
                    observed(swaps, cx),
                    "an unlocated recent setup keeps the polling pace"
                );
            });
        });
        let setup = issued.issued()[0].hash();
        executors
            .record_submission(operation, setup, B256::repeat_byte(80))
            .unwrap();
        let located_at =
            |swaps: &PrivateSwapsView| swaps.tracking.get(&operation).unwrap().located_at;
        let expired = || {
            Instant::now()
                .checked_sub(DEFERRED_SETUP_OBSERVATION_INTERVAL)
                .unwrap()
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = None;
                assert!(
                    !observed(swaps, cx),
                    "no confirmed block can contain a located setup sent after signing yet"
                );
            });
            set_head(100, cx);
            swaps.update(cx, |swaps, cx| {
                let handed_off = located_at(swaps).expect("the hand-off is recorded");
                assert_eq!(handed_off.0, setup);
                assert!(
                    !observed(swaps, cx),
                    "private sync locates a handed-off setup, so even its first read waits"
                );
                swaps.reload_records();
                assert_eq!(
                    located_at(swaps),
                    Some(handed_off),
                    "reloads keep the hand-off time"
                );
                swaps.tracking.get_mut(&operation).unwrap().located_at = Some((setup, expired()));
                assert!(observed(swaps, cx), "a fallback read still happens");
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = Some(Instant::now());
                assert!(!observed(swaps, cx), "the last read restarts the wait");
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = Some(expired());
            });
        });
        // A replacement attempt at the same nonce waits again after its own hand-off.
        let retry_observed =
            ExecutorNonceObservation::new(BlockNumHash::new(20, B256::repeat_byte(20)), U256::ZERO);
        executors.reconcile(operation, retry_observed, &[]).unwrap();
        let replacement = B256::repeat_byte(6);
        executors
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    issued.delegate(),
                    replacement,
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"retry"),
                        retry_observed,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        executors
            .record_submission(operation, replacement, B256::repeat_byte(81))
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert_eq!(located_at(swaps).map(|(hash, _)| hash), Some(replacement));
                assert!(
                    !observed(swaps, cx),
                    "the replacement's hand-off defers its first read"
                );
            });
        });
        // An effect-less inclusion still needs the account check that reports it.
        let observed_after = |nonce| {
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), nonce)
        };
        executors
            .reconcile(
                operation,
                observed_after(U256::ONE),
                &[(
                    replacement,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(25, B256::repeat_byte(25)),
                        B256::repeat_byte(81),
                        ExecutorExecutionResult::MissingEffects,
                    ),
                )],
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(woke(swaps), "a newly recorded inclusion wakes polling");
                assert!(
                    observed(swaps, cx),
                    "an effect-less inclusion is checked at once"
                );
            });
        });
        // The earlier attempt won the nonce. Confirmation observation records that from
        // private sync's location without a nonce observation, and the swap is ready to
        // place its approved order without another account read.
        executors
            .reconcile(
                operation,
                observed_after(U256::ONE),
                &[(
                    setup,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(80),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(woke(swaps), "a newly recorded inclusion wakes polling");
                let record = swaps.record(operation).unwrap();
                assert!(record.nonce_observation().is_none());
                assert!(matches!(
                    record.recorded_payload_status(replacement),
                    Some(ExecutorPayloadStatus::Invalidated { .. })
                ));
                assert_eq!(
                    swaps.tracking.get(&operation).unwrap().setup,
                    Some(wallet_ops::SwapSetupStatus::Pending)
                );
                assert_eq!(swaps.stage(record), SwapStage::Approved);
                assert!(
                    !observed(swaps, cx),
                    "a recorded executed setup needs no setup page"
                );
                swaps.reload_records();
                assert!(
                    !woke(swaps),
                    "an already known inclusion does not wake polling again"
                );
            });
        });
    });
}

#[gpui::test]
fn dismissed_expired_swap_stays_dormant_after_restart(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorInputIdentity, ExecutorNonceObservation,
        ExecutorPayloadContext, ExecutorPayloadInclusion, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation, SwapOrderObservations,
        SwapPreHookDeath, SwapPreHookDeathCause, SwapProof, SwapRecipient, SwapTerms,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let setup = pending_setup(executors, operation);
        let setup_hash = setup.issued()[0].hash();
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
        executors
            .reconcile(
                operation,
                observed,
                &[(
                    setup_hash,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
            "tree": 4, "position": 16198, "commitment": "0x2"
        }))
        .unwrap();
        let uid = OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), 20);
        let hook = |nonce, hash, purpose, inputs| {
            IssuedExecutorPayload::new(
                U256::from(nonce),
                setup.delegate(),
                B256::repeat_byte(hash),
                purpose,
                ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
            )
        };
        executors
            .record_swap_attempt(
                operation,
                SwapAttempt {
                    terms: SwapTerms::new(
                        Address::repeat_byte(1),
                        Address::repeat_byte(2),
                        SwapRecipient::new(U256::ONE, [7; 32]),
                        setup_hash,
                    ),
                    proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                    uid,
                    submission: None,
                    delivery: SwapDelivery::Reshield,
                    bounds: test_approval().bounds,
                    invalidates: None,
                    pre_hook: hook(
                        1_u64,
                        7,
                        ExecutorPayloadPurpose::SwapPreHook,
                        vec![input.clone()],
                    ),
                    post_hook: hook(2_u64, 8, ExecutorPayloadPurpose::SwapPostHook, Vec::new()),
                },
            )
            .unwrap();
        let record = executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    pre_hook_dead: Some(SwapPreHookDeath {
                        cause: SwapPreHookDeathCause::Expired,
                        observation: SwapObservation {
                            block: observed.block(),
                            transaction_hash: None,
                        },
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!record.reserved_inputs().contains(&input));
        executors.set_hidden(operation, true).unwrap();
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(100);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(record.nonce_observation(), Some(observed));
                assert!(!record.reserved_inputs().contains(&input));
                assert!(!swaps.has_shown_swaps());
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "completed swaps must not trigger account-specific RPC after restart"
                );
                // Removed from the Private tab, the swap stays in My orders.
                swaps.show_view(dialog::SwapDialogView::Orders, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let row = cx
            .debug_bounds(format!("swap-order-row-{}", operation.opaque_id()).leak())
            .expect("a removed swap stays listed in My orders");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            assert_eq!(
                swaps.read(cx).dialog.as_ref().map(|dialog| dialog.view),
                Some(dialog::SwapDialogView::Detail(operation)),
                "selecting it opens its detail"
            );
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("swap-progress-remove").is_none(),
            "an already removed swap isn't offered for removal again"
        );

        // Older records may have lost their nonce evidence during an interrupted check.
        // Keep those notes locked, but don't turn startup into a migration RPC sweep.
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(
                    swaps
                        .record(operation)
                        .unwrap()
                        .reserved_inputs()
                        .contains(&input)
                );
                assert!(swaps.next_observations(cx).is_none());
            });
        });

        let pending = ExecutorOperationId::random().unwrap();
        executors
            .reserve(pending, setup.delegate(), Some("Private swap"), &[])
            .unwrap();
        executors
            .bind_address(pending, Address::repeat_byte(9))
            .unwrap();
        let pending_nonce = ExecutorNonceObservation::new(observed.block(), U256::ZERO);
        executors.reconcile(pending, pending_nonce, &[]).unwrap();
        executors
            .record_issued(
                pending,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    setup.delegate(),
                    B256::repeat_byte(9),
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"setup"),
                        pending_nonce,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let (_, _, pages) = swaps.next_observations(cx).expect("resume pending setup");
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].operation, pending);
            });
        });
    });
}

#[gpui::test]
fn private_tab_details_opens_the_swap_or_the_open_orders(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_details(window, cx);
                assert_eq!(
                    swaps.dialog.as_ref().map(|dialog| dialog.view),
                    Some(dialog::SwapDialogView::Detail(operation))
                );
            });
            window.draw(cx).clear(cx);
        });
        // With several swaps on the Private tab, Details… lists the open ones.
        let second = ExecutorOperationId::random().unwrap();
        let delegate = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
                .delegate()
        });
        executors
            .reserve(
                second,
                delegate,
                Some("Private swap"),
                &[
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                ],
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.open_details(window, cx);
                assert_eq!(
                    swaps.dialog.as_ref().map(|dialog| dialog.view),
                    Some(dialog::SwapDialogView::Orders)
                );
                assert_eq!(swaps.orders_filter, Some(model::SwapOrderGroup::Open));
            });
            window.draw(cx).clear(cx);
        });
        for shown in [operation, second] {
            assert!(
                cx.debug_bounds(format!("swap-order-row-{}", shown.opaque_id()).leak())
                    .is_some()
            );
        }
    });
}

fn test_approval() -> SwapApproval {
    SwapApproval {
        bounds: wallet_ops::vault::SwapApprovedBounds {
            sell_amount: U256::from(100),
            unshield_amount: None,
            unshield_fee_bps: U256::ZERO,
            buy_amount: U256::from(99),
            private_minimum: U256::from(98),
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 1_000_000,
            post_hook_gas_limit: 1_000_000,
            hook_cost: Some(U256::ONE),
            anchors: Vec::new(),
        },
        price_verified: Some(false),
        price_acknowledged: true,
    }
}

fn with_swap_view(
    cx: &mut TestAppContext,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    // Parallel fixtures can receive the same wall-clock timestamp. Reserve the
    // directory atomically before opening its database.
    let directory = tempfile::Builder::new()
        .prefix("swap-ui-")
        .tempdir()
        .unwrap();
    let path = directory.path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(|cx| {
        gpui_component::init(cx);
        ui::theme::apply_zenburn_component_theme(cx);
    });
    let mut fixture = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let root = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        let view = root.read(cx).view_session.clone().unwrap();
        let vault = root.read(cx).vault_store.clone().unwrap();
        let mut chain = root
            .read(cx)
            .effective_chain_configs
            .get(1)
            .unwrap()
            .clone();
        chain.rpc_route = wallet_ops::RpcChainRoute::new(
            1,
            vec!["http://127.0.0.1:1".parse::<reqwest::Url>().unwrap()],
        );
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        let poi = wallet_ops::PoiReadSource::PoiProxy {
            rpc_url: "http://127.0.0.1:1".parse::<reqwest::Url>().unwrap().into(),
        };
        let store = wallet_ops::WalletSessionStore::from_db(vault.db(), poi.clone()).unwrap();
        let mut lifecycle = WalletSyncLifecycle::new();
        let registration = lifecycle.prepare_startup(1);
        let session = runtime.block_on(async {
            let http = wallet_ops::build_wallet_network_context(wallet_ops::WalletNetworkConfig {
                network_mode: Some(wallet_ops::WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &path,
            })
            .await
            .unwrap();
            Box::pin(store.start_view_wallet_session_immediate(
                wallet_ops::ViewWalletChainSessionRequest {
                    view_session: view.clone(),
                    wallet_scope_generation: registration.generation,
                    chain_id: 1,
                    effective_chain: chain.clone(),
                    sync_start_policy:
                        wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                    init_block_number: Some(0),
                    sync_to_block: Some(0),
                    use_indexed_wallet_catch_up: false,
                    poi_read_source: poi,
                    rewind_wallet_cache: false,
                    progress_tx: None,
                },
                &http,
            ))
            .await
            .unwrap()
        });
        let session = Arc::new(session);
        let operation = ExecutorOperationId::random().unwrap();
        let executors = ExecutorStore::new(vault.db(), view, 1).unwrap();
        executors
            .reserve(
                operation,
                chain.accepted_executor_profile().unwrap().delegate(),
                Some("Private swap"),
                &[
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                ],
            )
            .unwrap();
        let observation = session.observation_rx.borrow().clone();
        root.update(cx, |root, _| {
            root.chain_states.insert(
                1,
                ChainUtxoState::Ready {
                    snapshot: observation.snapshot,
                    session: session.clone(),
                    observer_token: registration.observer_token,
                    sync_tip: wallet_ops::WalletSyncTip::default(),
                    poi_refreshing: false,
                    ppoi_workflow_status: observation.ppoi_workflow_status,
                },
            );
        });
        fixture = Some((root.clone(), store, session, executors, operation));
        let view = cx.new(|_| WalletWindow(root));
        Root::new(view, window, cx)
    });
    let (root, store, session, executors, operation) = fixture.unwrap();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let swaps = root.read_with(cx, |root, _| root.private_swaps_view().unwrap());
    assert!(swaps.read_with(cx, |swaps, _| swaps.has_shown_swaps()));

    test(&root, &swaps, &executors, operation, &runtime, cx);
    cx.update(|window, _| window.remove_window());
    drop(swaps);
    drop(host);
    drop(root);
    cx.run_until_parked();
    runtime.block_on(async {
        session.stop().await.unwrap();
        store.shutdown().await;
    });
    drop(session);
    drop(store);
    drop(executors);
    drop(entered);
    drop(runtime);
    directory.close().unwrap();
}

#[gpui::test]
fn place_order_keeps_progress_visible_while_checking_terms_and_after_failure(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        use alloy::eips::{BlockNumHash, eip7702::constants::EIP7702_DELEGATION_DESIGNATOR};
        use alloy::primitives::{B256, Bytes};
        use wallet_ops::vault::{
            ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
            ExecutorPayloadInclusion, ExecutorPayloadPurpose, IssuedExecutorPayload,
        };

        let profile = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
        });
        executors
            .bind_address(operation, Address::repeat_byte(3))
            .unwrap();
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
        executors.reconcile(operation, observed, &[]).unwrap();
        let payload = B256::repeat_byte(4);
        executors
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    profile.delegate(),
                    payload,
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(Bytes::from_static(b"setup"), observed, Vec::new()),
                ),
            )
            .unwrap();
        let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
        let record = executors
            .reconcile(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::ONE),
                &[(
                    payload,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        let setup = wallet_ops::swap_setup_status(&record, confirmed, &code, profile);
        let approval = test_approval();
        executors
            .record_swap_approval(operation, approval.clone())
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().setup = Some(setup);
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::Approved
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let button = cx.debug_bounds("swap-progress-continue").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "progress stays visible while getting the quote"
            );
            swaps.update(cx, |swaps, cx| {
                let job = swaps
                    .job
                    .take()
                    .expect("recorded setup can start a quote without a freshly synced chain head");
                assert_eq!(job.kind, SwapJobKind::Requote);
                // Replace the network work with a controlled response. Its cancelled
                // completion must not overwrite the next retry's state.
                job.abort.abort();
                swaps.job_revision += 1;
                swaps.apply_approved_quote(
                    operation,
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        outcome: Err(eyre::eyre!("initial quote unavailable")),
                    },
                    window,
                    cx,
                );
                assert!(swaps.tracking.get(&operation).unwrap().error.is_some());
            });
            assert!(
                window.has_active_dialog(cx),
                "a quote failure stays visible"
            );
        });

        // Hold the quote response to exercise an arbitrarily slow request without using
        // live services or a timer. Starting a retry clears the previous failure.
        let (send, receive) = tokio::sync::oneshot::channel();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let approval = approval.clone();
                swaps.start_job(
                    operation,
                    SwapJobKind::Requote,
                    async move { Ok(receive.await.unwrap()) },
                    move |swaps, result, window, cx| {
                        swaps.apply_approved_quote(operation, &approval, result, window, cx);
                    },
                    window,
                    cx,
                );
                assert!(swaps.tracking.get(&operation).unwrap().error.is_none());
            });
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
        });
        send.send(QuoteResult {
            orderbook: None,
            outcome: Err(eyre::eyre!("quote unavailable")),
        })
        .ok()
        .unwrap();
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let swaps = swaps.read(cx);
            assert!(!swaps.busy());
            assert_eq!(
                swaps.tracking.get(&operation).unwrap().error.as_deref(),
                Some("quote unavailable")
            );
            assert!(
                window.has_active_dialog(cx),
                "a failed quote must leave progress open for retry"
            );
        });

        // Another dialog can open while the request is in flight. It must keep focus
        // and remain open, even when this swap's progress is still underneath it.
        cx.update(|window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Unrelated form"));
            let focus = window.focused(cx);
            swaps.update(cx, |swaps, cx| {
                swaps.apply_approved_quote(
                    operation,
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        outcome: Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates)),
                    },
                    window,
                    cx,
                );
                assert!(swaps.form.is_none());
                assert!(swaps.tracking.get(&operation).unwrap().auto_place);
                // A failed response still records its error and stops automatic retries.
                swaps.apply_approved_quote(
                    operation,
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        outcome: Err(eyre::eyre!("quote failed again")),
                    },
                    window,
                    cx,
                );
                let tracking = swaps.tracking.get(&operation).unwrap();
                assert!(!tracking.auto_place);
                assert_eq!(tracking.error.as_deref(), Some("quote failed again"));
            });
            assert_eq!(window.focused(cx), focus);
            window.close_dialog(cx);
            window.draw(cx).clear(cx);
        });

        // A successful quote that changes the approved terms must advance from this
        // swap's detail to review, rather than defer forever because a dialog is open.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.apply_approved_quote(
                    operation,
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        outcome: Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates)),
                    },
                    window,
                    cx,
                );
                assert_eq!(
                    swaps.form.as_ref().and_then(SwapForm::operation),
                    Some(operation)
                );
                assert!(swaps.reapproval.is_some());
            });
            window.draw(cx).clear(cx);
        });

        // Authorization starts work from the review form. A slow submission must
        // immediately hand over to progress, where failures remain retryable.
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(swaps.form.is_some());
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    async move {
                        receive.await.unwrap();
                        Err::<(), _>(eyre::eyre!("order unavailable"))
                    },
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
                assert!(swaps.form.is_none(), "the disabled form is replaced");
                assert!(swaps.detail_is_active(operation, window, cx));
                assert!(swaps.busy());
            });
            window.draw(cx).clear(cx);
        });
        send.send(()).unwrap();
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let swaps = swaps.read(cx);
            assert!(!swaps.busy());
            assert!(swaps.detail_is_active(operation, window, cx));
            assert_eq!(
                swaps.tracking.get(&operation).unwrap().error.as_deref(),
                Some("order unavailable")
            );
        });
    });
}

/// My orders focuses its list. With no row to show, the empty state holds that focus, so
/// Escape still reaches the dialog.
#[gpui::test]
fn my_orders_without_rows_keeps_focus_in_the_dialog(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let row: &'static str = format!("swap-order-row-{}", operation.opaque_id()).leak();
        let open_orders = |filter: Option<model::SwapOrderGroup>,
                           cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.orders_filter = filter;
                    swaps.show_view(dialog::SwapDialogView::Orders, window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };
        let escape_closes = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| assert!(swaps.read(cx).swap_dialog_active(window, cx)));
            cx.simulate_keystrokes("escape");
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(!window.has_active_dialog(cx));
                assert!(swaps.read(cx).dialog.is_none());
            });
        };
        // The only swap is open, so Ended lists nothing.
        open_orders(Some(model::SwapOrderGroup::Ended), cx);
        assert!(cx.debug_bounds(row).is_none());
        escape_closes(cx);
        // A reload can empty the group on screen while its list has focus.
        open_orders(Some(model::SwapOrderGroup::Open), cx);
        assert!(cx.debug_bounds(row).is_some());
        executors.retire(operation).unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                cx.notify();
            });
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds(row).is_none());
        escape_closes(cx);
    });
}

/// A swap whose unshield ran and whose order expired unfilled. Its sell token waits in the
/// stealth account, so Stealth accounts offers recovery without a balance check.
fn stranded_swap(executors: &ExecutorStore, operation: ExecutorOperationId) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorInputIdentity, ExecutorNonceObservation,
        ExecutorPayloadContext, ExecutorPayloadInclusion, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation, SwapOrderObservations,
        SwapProof, SwapRecipient, SwapTerms,
    };
    let setup = pending_setup(executors, operation);
    let setup_hash = setup.issued()[0].hash();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    executors
        .reconcile(
            operation,
            observed,
            &[(
                setup_hash,
                ExecutorPayloadInclusion::new(
                    BlockNumHash::new(11, B256::repeat_byte(11)),
                    B256::repeat_byte(5),
                    ExecutorExecutionResult::Executed,
                ),
            )],
        )
        .unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16198, "commitment": "0x2"
    }))
    .unwrap();
    let uid = OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), 20);
    let hook = |nonce, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            setup.delegate(),
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
        )
    };
    executors
        .record_swap_attempt(
            operation,
            SwapAttempt {
                terms: SwapTerms::new(
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    SwapRecipient::new(U256::ONE, [7; 32]),
                    setup_hash,
                ),
                proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                uid,
                submission: None,
                delivery: SwapDelivery::Reshield,
                bounds: test_approval().bounds,
                invalidates: None,
                pre_hook: hook(1_u64, 7, ExecutorPayloadPurpose::SwapPreHook, vec![input]),
                post_hook: hook(2_u64, 8, ExecutorPayloadPurpose::SwapPostHook, Vec::new()),
            },
        )
        .unwrap();
    let seen = SwapObservation {
        block: observed.block(),
        transaction_hash: None,
    };
    executors
        .record_swap_observations(
            operation,
            uid,
            SwapOrderObservations {
                pre_hook_executed: Some(seen),
                expired: Some(seen),
                ..Default::default()
            },
        )
        .unwrap();
}

#[gpui::test]
fn reused_account_progress_keeps_the_new_swap_separate_from_its_history(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation,
        SwapOrderObservations, SwapProof, SwapTerms, SwapTradeAmounts,
    };

    with_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        stranded_swap(executors, operation);
        let mut record = executors.records().unwrap().pop().unwrap();
        let previous = record.swap().unwrap().orders().last().unwrap().uid();
        let seen = SwapObservation {
            block: record.nonce_observation().unwrap().block(),
            transaction_hash: Some(B256::repeat_byte(40)),
        };
        record = executors
            .record_swap_observations(
                operation,
                previous,
                SwapOrderObservations {
                    pre_hook_executed: Some(seen),
                    traded: Some(seen),
                    delivered: Some(seen),
                    shielded: Some(
                        serde_json::from_value(serde_json::json!({
                            "observation": seen, "private_amount": "0x62", "fee": "0x1"
                        }))
                        .unwrap(),
                    ),
                    trade_amounts: Some(SwapTradeAmounts {
                        sell_amount: U256::from(100),
                        buy_amount: U256::from(99),
                        fee_amount: U256::ZERO,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        executors.set_hidden(operation, true).unwrap();
        let pending = PendingSwapOrder {
            previous_order: Some(previous),
            sell: Address::repeat_byte(2),
            buy: Address::repeat_byte(1),
            amount: U256::from(50),
            private_minimum: U256::from(45),
            slippage_bps: 100,
            reuse_account: true,
            started_at: now_unix(),
        };
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().pending_order = Some(pending);
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    async move {
                        receive.await.unwrap();
                        Err::<(), _>(eyre::eyre!("proof preparation failed"))
                    },
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::Order(SwapOrderState::Done));
                assert_eq!(swaps.progress_stage(record), SwapStage::Ready);
                assert_eq!(
                    swaps.progress_title(operation, cx),
                    format!(
                        "Swap {} for {}",
                        swaps.token_amount(pending.sell, pending.amount, cx),
                        swaps.token_symbol(pending.buy, cx)
                    ),
                );
                assert!(
                    swaps.has_shown_swaps(),
                    "a previously hidden account shows new work"
                );
                assert_eq!(swaps.past_swap(operation, 0).unwrap().2.uid(), previous);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());

        // The previous result remains accessible, but closing and reopening the current
        // swap from Private must keep showing the new submission.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.show_view(dialog::SwapDialogView::PastDetail(operation, 0), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_some());
        cx.update(|window, cx| {
            window.close_all_dialogs(cx);
            swaps.update(cx, |swaps, cx| swaps.open_details(window, cx));
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());

        // A failure before persistence must retry the new terms, not the completed swap.
        send.send(()).unwrap();
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(!swaps.busy());
                assert_eq!(
                    swaps.progress_stage(swaps.record(operation).unwrap()),
                    SwapStage::Ready
                );
                swaps.open_existing_form(operation, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (pending.sell, Some(pending.buy)));
                assert_eq!(swaps.form_amount(form, cx).unwrap(), pending.amount);
                assert_eq!(form.slippage_bps, pending.slippage_bps);
                assert!(form.reuse_account);
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    std::future::pending::<eyre::Result<()>>(),
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
            });
        });

        // Persist the second order while submission is still in flight. Its UID and durable
        // progress immediately replace the preparation view; the old result stays separate.
        let observed = ExecutorNonceObservation::new(
            BlockNumHash::new(40, B256::repeat_byte(40)),
            U256::from(3),
        );
        let setup = &record.issued()[0];
        executors
            .reconcile(
                operation,
                observed,
                &[(setup.hash(), setup.inclusion().unwrap())],
            )
            .unwrap();
        let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
            "tree": 4, "position": 16199, "commitment": "0x3"
        }))
        .unwrap();
        let hook = |nonce, hash, purpose, inputs| {
            IssuedExecutorPayload::new(
                U256::from(nonce),
                record.delegate(),
                B256::repeat_byte(hash),
                purpose,
                ExecutorPayloadContext::new(Bytes::from_static(b"new hook"), observed, inputs),
            )
        };
        let uid = OrderUid::new(B256::repeat_byte(0x12), Address::repeat_byte(3), u32::MAX);
        let mut bounds = test_approval().bounds;
        bounds.sell_amount = pending.amount;
        bounds.private_minimum = pending.private_minimum;
        let terms = record.swap().unwrap().terms();
        executors
            .record_swap_attempt(
                operation,
                SwapAttempt {
                    terms: SwapTerms::new(
                        pending.sell,
                        pending.buy,
                        terms.recipient(),
                        setup.hash(),
                    ),
                    proof: SwapProof::new(B256::repeat_byte(9), vec![input.clone()]),
                    uid,
                    submission: None,
                    delivery: SwapDelivery::Reshield,
                    bounds,
                    invalidates: None,
                    pre_hook: hook(3_u64, 9, ExecutorPayloadPurpose::SwapPreHook, vec![input]),
                    post_hook: hook(4_u64, 10, ExecutorPayloadPurpose::SwapPostHook, Vec::new()),
                },
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert!(swaps.busy());
                assert!(swaps.pending_order(record).is_none());
                assert_eq!(swaps.progress_stage(record), SwapStage::SubmissionPending);
                assert_eq!(record.swap().unwrap().orders().last().unwrap().uid(), uid);
                assert_eq!(swaps.past_swap(operation, 0).unwrap().2.uid(), previous);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());
    });
}

/// Open the swap's detail and press Recover…, which reveals the account in Stealth accounts
/// behind the dialogs and then asks recovery to open.
fn press_swap_recover(
    swaps: &Entity<PrivateSwapsView>,
    operation: ExecutorOperationId,
    stage: SwapStage,
    cx: &mut gpui::VisualTestContext,
) {
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.reload_records();
            assert_eq!(swaps.stage(swaps.record(operation).unwrap()), stage);
            swaps.show_detail(operation, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    let recover = cx.debug_bounds("swap-progress-recover").unwrap();
    cx.simulate_click(recover.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

/// Closing recovery returns focus to the swap's detail under it, not to the account revealed
/// behind both dialogs.
#[gpui::test]
fn closing_swap_recovery_returns_focus_to_the_swap_detail(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        stranded_swap(executors, operation);
        press_swap_recover(
            swaps,
            operation,
            SwapStage::Order(wallet_ops::SwapOrderState::PreHookOnly { expired: true }),
            cx,
        );
        assert!(cx.debug_bounds("stealth-recovery-form").is_some());
        cx.update(|window, cx| {
            let swaps = swaps.read(cx);
            assert!(swaps.dialog.is_some());
            assert!(!swaps.swap_dialog_active(window, cx));
        });
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(swaps.read(cx).detail_is_active(operation, window, cx));
        });
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        // The dialog layer restores the focus recorded when recovery opened, after its close
        // animation.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
            assert!(swaps.read(cx).detail_is_active(operation, window, cx));
        });
    });
}

/// Recovery can decline after the reveal, here for a retired setup whose balances were never
/// checked. The swap's detail keeps focus.
#[gpui::test]
fn declined_swap_recovery_leaves_focus_in_the_swap_detail(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        executors.retire(operation).unwrap();
        press_swap_recover(swaps, operation, SwapStage::SetupRetired, cx);
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        cx.update(|window, cx| {
            assert!(swaps.read(cx).detail_is_active(operation, window, cx));
        });
    });
}

#[gpui::test]
fn routine_order_polling_waits_for_a_settlement_hint_without_reconciling_history(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        stranded_swap(executors, operation);
        let record = executors.records().unwrap().pop().unwrap();
        let uid = record.swap().unwrap().orders()[0].uid();
        executors
            .record_swap_observations(
                operation,
                uid,
                wallet_ops::vault::SwapOrderObservations::default(),
            )
            .unwrap();
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture")
                };
                sync_tip.head_block = Some(400);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let confirmed = swaps.confirmed_block(cx).unwrap();
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "no account RPC while waiting for CoW"
                );
                assert!(
                    swaps.next_order_hints(cx).is_some(),
                    "an order filled while offline must still be located after expiry"
                );
                let result = |block| HintResult {
                    operation,
                    uid,
                    client: None,
                    report: Some((
                        CowOrderStatusReport {
                            status: CowOrderStatusHint::Fulfilled,
                        },
                        Some(block),
                    )),
                };
                swaps.apply_order_hints(vec![result(confirmed + 1)], cx);
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "wait for safe depth locally"
                );
                swaps.apply_order_hints(vec![result(confirmed)], cx);
                let (_, _, pages) = swaps.next_observations(cx).unwrap();
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].settlement, Some((uid, confirmed)));
                assert!(!pages[0].setup);
                assert!(
                    pages[0].range.end > confirmed,
                    "no immediate account-history catchup loop"
                );
                assert!(
                    swaps.next_order_hints(cx).is_some(),
                    "a hint remains refreshable until verified"
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("swap-progress-check").is_some(),
            "an expired unresolved order offers an explicit account check"
        );

        // Once verified complete, the order's open detail asks the orderbook nothing more.
        let seen = wallet_ops::vault::SwapObservation {
            block: record.nonce_observation().unwrap().block(),
            transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
        };
        executors
            .record_swap_observations(
                operation,
                uid,
                wallet_ops::vault::SwapOrderObservations {
                    pre_hook_executed: Some(seen),
                    traded: Some(seen),
                    delivered: Some(seen),
                    shielded: Some(
                        serde_json::from_value(serde_json::json!({
                            "observation": seen, "private_amount": "0x62", "fee": "0x1"
                        }))
                        .unwrap(),
                    ),
                    ..Default::default()
                },
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().order_hint = None;
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::Order(SwapOrderState::Done));
                assert!(swaps.detail_is_active(operation, window, cx));
                assert!(
                    swaps.next_order_hints(cx).is_none(),
                    "a completed order's detail must not ask the orderbook about it"
                );
            });
        });
    });
}
