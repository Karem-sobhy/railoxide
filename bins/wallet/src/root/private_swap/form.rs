//! The swap form: tokens, amount and slippage, the setup's broadcaster route, the quote with
//! its price check, the single review, and placing the approved order.
//!
//! A new swap is quoted before anything is paid, with a stand-in executor that has no code, as
//! a fresh stealth account has none. One review approves the setup and the private minimum.
//! The wallet then sends the setup and, once it's confirmed, quotes again for the delegated
//! stealth account. If the shield fee, the price check and the minimum still hold, a
//! confirm-only step places the order; otherwise the review opens again. Retries and changed
//! terms use the form without a setup.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use gpui::{
    Anchor, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Colorize as _, Disableable as _, Icon, IconName, Sizable as _,
    WindowExt as _,
    alert::Alert,
    button::{ButtonGroup, ButtonVariants as _},
    checkbox::Checkbox,
    collapsible::Collapsible,
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    input::{Enter as InputEnter, InputEvent, InputState},
    popover::Popover,
    select::{Caret, SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    spinner::Spinner,
    tooltip::Tooltip,
};
use ui::controls::{
    app_amount_input, app_amount_text, app_button, app_button_base, app_button_label,
    app_muted_text, app_segment_button, app_strong_text, app_text,
};
use ui::theme;
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, ExecutorRecoveryFeeEstimate,
    OperationNetworkIsolation, PublicBroadcasterCandidate, PublicBroadcasterResultKind,
    PublicBroadcasterSelection, QuoteDeviationError, SwapAmountPlan, SwapAmountRequest,
    SwapExecutor, SwapOrderOutcome, SwapOrderRequest, SwapPrice, SwapReview, SwapReviewChange,
    SwapReviewRequest, SwapSetupRequest, TokenAnchorRateCache, TransactionGenerationStage,
    WakuDeliveryClient, WalletSession,
    cow::{CowOrderbookClient, OrderLimitError},
    default_public_broadcaster_fee_limit,
    settings::{
        EffectiveChainConfig, EffectiveTokenRegistry, SwapTokenEligibility, swap_destination_tokens,
    },
    vault::{ExecutorOperationId, ExecutorRecord, SwapApproval},
};

use super::dialog::{SwapDialogView, powered_by_cow};
use super::model::{
    SwapFormMode as FormMode, SwapStage, format_bps_percent, quote_anchor_delta_bps, swap_cost_bps,
    swap_form_mode, swap_total_cost,
};
use super::{
    PrivateSwapsView, SWAP_BROADCASTER_REPUBLISH_INTERVAL, SWAP_BROADCASTER_RESPONSE_TIMEOUT,
    SwapAction, SwapJobKind, swap_sell_amount, swap_tokens,
};
use crate::root::broadcaster_picker::{
    BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL, BroadcasterChoice,
    BroadcasterPickerFeeEstimateContext, BroadcasterPickerTarget, broadcaster_candidate_label,
    selected_broadcaster_label,
};
use crate::root::private_action::{
    PrivateActionAssetSelectItem, UnshieldAsset, fee_token_selector,
    private_action_asset_select_items,
};
use crate::root::public_broadcaster::{
    PublicBroadcasterFeeTokenOption, public_broadcaster_fee_token_options_from_snapshot,
    resolve_selected_public_broadcaster_fee_token,
};
use crate::root::spend_authorization::{
    SpendAuthorizationAsset, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
};
use crate::root::stealth_accounts::{RecoveryPickerContext, same_offer};
use crate::root::{
    COST_ESTIMATE_DEBOUNCE, DeliveryFormKind, format_token_amount_ceiling_for_display,
    format_unshield_amount_input, format_value_with_usd_label, token_display_metadata,
};

const SLIPPAGE_CHOICES: [(u32, &str); 4] = [(10, "0.1%"), (50, "0.5%"), (100, "1%"), (300, "3%")];
const DEFAULT_SLIPPAGE_BPS: u32 = 50;
const QUOTE_DEBOUNCE: Duration = Duration::from_millis(600);
// Correlates overlapping quote attempts without logging a wallet or operation identifier.
static NEXT_QUOTE_TRACE_ID: AtomicU64 = AtomicU64::new(1);
/// The stealth account row's label column, in rems; the line under the select starts past it.
const ACCOUNT_LABEL_WIDTH: f32 = 7.5;
const ACCOUNT_REUSE_NOTE: &str = "Reusing this public address can link this swap to its previous activity and reduce your privacy. A new stealth account offers more privacy.";
const UNVERIFIED_PRICE_WARNING: &str = "Price couldn't be independently verified.";

/// A swap the user approved in one review: its setup through a broadcaster's private fee, and
/// the order's terms, placed once the setup is confirmed.
#[derive(Clone)]
pub(super) struct SetupApproval {
    pub(super) operation: ExecutorOperationId,
    /// The stealth account is already reserved; set it up again with the same account.
    resume: bool,
    sell: Address,
    buy: Address,
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
    waku: Arc<WakuDeliveryClient>,
    /// Persisted with the setup: the approved amount, minimum, fee and price check.
    approval: SwapApproval,
    orderbook: Option<CowOrderbookClient>,
}

/// The order terms the user approved, for a stealth account that is set up.
#[derive(Clone)]
pub(super) struct OrderApproval {
    pub(super) operation: ExecutorOperationId,
    review: Arc<SwapReview>,
    /// The reviewed suggestion, or the minimum approved with the setup.
    private_minimum: U256,
    price_acknowledged: bool,
    orderbook: CowOrderbookClient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PriceBlock {
    /// The quote is worse than the anchor price by more than the profile allows.
    Deviates,
}

enum QuoteState {
    Idle,
    Loading,
    /// The amount doesn't fit one swap; `largest` does.
    TooLarge {
        largest: U256,
    },
    Ready(Arc<SwapReview>),
    PriceBlocked(PriceBlock),
    Failed(eyre::Report),
}

#[derive(Default)]
struct SetupRoute {
    fee_token: Option<Address>,
    fee_options: Vec<PublicBroadcasterFeeTokenOption>,
    candidates: Vec<PublicBroadcasterCandidate>,
    selected: Option<String>,
    allow_out_of_range: bool,
    favorites_only: bool,
    estimate: Option<ExecutorRecoveryFeeEstimate>,
    estimate_candidate: Option<PublicBroadcasterCandidate>,
    estimate_error: Option<String>,
    estimate_task: Option<Task<()>>,
    estimate_revision: u64,
    next_estimate: Option<Instant>,
    refresh_task: Option<Task<()>>,
}

impl SetupRoute {
    fn invalidate_estimate(&mut self) {
        self.estimate = None;
        self.estimate_candidate = None;
        self.estimate_error = None;
        self.estimate_task = None;
        self.next_estimate = None;
        self.estimate_revision = self.estimate_revision.wrapping_add(1);
    }

    fn choice(&self) -> BroadcasterChoice {
        self.selected
            .clone()
            .map_or(BroadcasterChoice::Random, |railgun_address| {
                BroadcasterChoice::Specific { railgun_address }
            })
    }
}

/// A new swap's stealth account: a new one, or a set-up account to use again.
#[derive(Clone)]
struct SwapAccountSelectItem {
    /// `None` for a new stealth account.
    operation: Option<ExecutorOperationId>,
    address: Option<Address>,
    label: SharedString,
}

impl SelectItem for SwapAccountSelectItem {
    type Value = Option<ExecutorOperationId>;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.operation
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        self.label.to_ascii_lowercase().contains(&query)
            || self.address.is_some_and(|address| {
                address
                    .to_checksum(None)
                    .to_ascii_lowercase()
                    .contains(&query)
            })
    }
}

pub(super) struct SwapForm {
    /// The swap's executor operation once a setup was approved, or the swap being retried.
    operation: Option<ExecutorOperationId>,
    reuse_account: bool,
    /// A new swap's choice of stealth account. A started swap keeps its own and has none.
    account_select: Option<Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>>,
    sell: Address,
    buy: Option<Address>,
    sell_select: Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
    buy_select: Entity<ComboboxState<SearchableVec<PrivateActionAssetSelectItem>>>,
    amount_input: Entity<InputState>,
    slippage_bps: u32,
    route: SetupRoute,
    quote: QuoteState,
    quote_task: Option<Task<()>>,
    quote_revision: u64,
    /// The route of this form's quotes, kept for the swap's order once it has an operation.
    orderbook: Option<CowOrderbookClient>,
    price_acknowledged: bool,
    /// Applies only to the current form quote; editing or retrying clears it.
    high_costs_acknowledged: bool,
    error: Option<String>,
    /// The order detail this form continues or retries, which Back returns to.
    back_to_detail: Option<ExecutorOperationId>,
    details_open: bool,
    /// The setup broadcaster popover. Presses elsewhere in the form close it, so a fee token
    /// list opened inside it stays usable.
    settings_open: bool,
    /// The "You receive" hint pinned open as a popover. A new quote closes it.
    receive_help_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl SwapForm {
    pub(super) const fn operation(&self) -> Option<ExecutorOperationId> {
        self.operation
    }

    fn review_problem(&self, review: &SwapReview) -> Option<&'static str> {
        if *review.price() == SwapPrice::Unverified && !self.price_acknowledged {
            Some("Accept the unverified price before you review the swap.")
        } else if high_cost_bps(review).is_some() && !self.high_costs_acknowledged {
            Some("Confirm Swap anyway to accept the high swap costs.")
        } else {
            None
        }
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }
}

/// The executor a quote is planned for.
#[derive(Clone, Copy)]
enum QuoteExecutor {
    /// Before the setup: a stand-in with no code, like a fresh stealth account.
    Preview,
    /// A setup retry may reuse this operation's reserved fee inputs.
    Setup(ExecutorOperationId),
    /// Account state recorded by observation; refreshed before execution preparation.
    Order {
        operation: ExecutorOperationId,
        reuse: bool,
    },
}

struct QuoteRequest {
    executor: QuoteExecutor,
    sell: Address,
    buy: Address,
    amount: U256,
    slippage_bps: u32,
    byte_budget: Option<usize>,
    orderbook: Option<CowOrderbookClient>,
    anchor_cache: Option<Arc<TokenAnchorRateCache>>,
    tokens: EffectiveTokenRegistry,
}

enum QuoteOutcome {
    TooLarge(U256),
    Review(Box<SwapReview>),
    PriceBlocked(PriceBlock),
}

struct QuoteResult {
    orderbook: Option<CowOrderbookClient>,
    outcome: eyre::Result<QuoteOutcome>,
}

enum SetupOutcome {
    /// The broadcaster's answer for the setup.
    Sent(PublicBroadcasterResultKind),
    /// The approved amount no longer fits one order; nothing was paid.
    TooLarge,
}

/// Plan the notes without proving, for a stand-in executor before setup or from recorded
/// account state, then quote without hooks and price the order
/// limit. Nothing is signed. The swap's orderbook route is returned for reuse even when
/// quoting fails.
#[tracing::instrument(
    name = "swap_quote",
    target = "swap_quote",
    level = "debug",
    skip_all,
    fields(quote_id = NEXT_QUOTE_TRACE_ID.fetch_add(1, Ordering::Relaxed))
)]
async fn quote_swap(
    owner: Arc<ExecutorOwner>,
    session: Arc<WalletSession>,
    request: QuoteRequest,
) -> QuoteResult {
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "total", "started");
    let mut orderbook = request.orderbook.clone();
    let outcome = quote_swap_terms(&owner, &session, &request, &mut orderbook).await;
    let status = match &outcome {
        Ok(QuoteOutcome::Review(_)) => "ready",
        Ok(QuoteOutcome::TooLarge(_)) => "too_large",
        Ok(QuoteOutcome::PriceBlocked(_)) => "price_blocked",
        Err(_) => "error",
    };
    tracing::debug!(
        target: "swap_quote",
        step = "total",
        elapsed_ms = started.elapsed().as_millis(),
        status,
        "finished"
    );
    QuoteResult { orderbook, outcome }
}

async fn quote_swap_terms(
    owner: &Arc<ExecutorOwner>,
    session: &Arc<WalletSession>,
    request: &QuoteRequest,
    orderbook: &mut Option<CowOrderbookClient>,
) -> eyre::Result<QuoteOutcome> {
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "executor", "started");
    let executor = match request.executor {
        QuoteExecutor::Preview => owner.swap_preview_executor()?,
        QuoteExecutor::Setup(operation) => owner.swap_setup_preview(operation)?,
        QuoteExecutor::Order { operation, reuse } => owner.swap_order_preview(operation, reuse)?,
    };
    tracing::debug!(
        target: "swap_quote",
        step = "executor",
        elapsed_ms = started.elapsed().as_millis(),
        "finished"
    );
    let started = Instant::now();
    tracing::debug!(
        target: "swap_quote",
        step = "orderbook_client",
        reused = orderbook.is_some(),
        "started"
    );
    let client = if let Some(client) = orderbook.clone() {
        client
    } else {
        let client = Box::pin(owner.swap_orderbook_client()).await?;
        *orderbook = Some(client.clone());
        client
    };
    tracing::debug!(
        target: "swap_quote",
        step = "orderbook_client",
        elapsed_ms = started.elapsed().as_millis(),
        "finished"
    );
    let amount_request = SwapAmountRequest {
        sell_token: request.sell,
        buy_token: request.buy,
        amount: request.amount,
        byte_budget: request.byte_budget,
    };
    let planning_owner = Arc::clone(owner);
    let planning_session = Arc::clone(session);
    // Note selection can take a moment on fragmented balances.
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "input_planning", "started");
    let plan = tokio::task::spawn_blocking(move || {
        planning_owner.plan_swap_amount(&executor, &planning_session, &amount_request)
    })
    .await;
    tracing::debug!(
        target: "swap_quote",
        step = "input_planning",
        elapsed_ms = started.elapsed().as_millis(),
        success = matches!(&plan, Ok(Ok(_))),
        "finished"
    );
    let plan = plan.map_err(|_| eyre::eyre!("Planning the swap stopped unexpectedly."))??;
    let plan = match plan {
        SwapAmountPlan::TooLarge { largest } => {
            return Ok(QuoteOutcome::TooLarge(largest.amount()));
        }
        SwapAmountPlan::Fits(plan) => plan,
    };
    match Box::pin(owner.review_swap(SwapReviewRequest {
        plan,
        slippage_bps: request.slippage_bps,
        orderbook: &client,
        anchor_cache: request.anchor_cache.as_deref(),
        token_registry: &request.tokens,
    }))
    .await
    {
        Ok(review) => Ok(QuoteOutcome::Review(Box::new(review))),
        Err(error)
            if matches!(
                error.downcast_ref::<QuoteDeviationError>(),
                Some(QuoteDeviationError::ExceedsThreshold)
            ) =>
        {
            Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates))
        }
        Err(error) => Err(error),
    }
}

impl PrivateSwapsView {
    pub(super) fn open_new_form(
        &mut self,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        self.open_form(None, sell, None, None, None, window, cx);
    }

    /// Explicit account selection keeps normal swaps on their fresh-account path.
    pub(in crate::root) fn open_account_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let record = self.owner.records().ok().and_then(|records| {
            records
                .into_iter()
                .find(|record| record.operation() == operation)
        });
        let Some(record) = record.filter(|record| record.address().is_some()) else {
            return;
        };
        let sell = swap_tokens(&record).map(|(sell, _)| sell).or_else(|| {
            self.sell_assets(Some(operation), cx)
                .first()
                .map(|asset| asset.token)
        });
        let Some(sell) = sell else {
            return;
        };
        self.open_form(None, sell, None, None, None, window, cx);
        // Offer this account even when it can't swap now; its check explains why.
        let items = self.swap_account_items(Some(&record), cx);
        if let Some(select) = self
            .form
            .as_ref()
            .and_then(|form| form.account_select.clone())
        {
            select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(items), window, cx);
            });
        }
        self.select_form_account(Some(operation), window, cx);
    }

    /// The accounts a new swap can use: a new one first, then set-up accounts, newest first.
    /// `chosen` is listed even when the local rules leave it out.
    fn swap_account_items(
        &self,
        chosen: Option<&ExecutorRecord>,
        cx: &App,
    ) -> Vec<SwapAccountSelectItem> {
        let mut accounts = self
            .owner
            .swap_account_candidates()
            .unwrap_or_default()
            .into_iter()
            .map(|candidate| {
                (
                    candidate.operation(),
                    candidate.index(),
                    candidate.address(),
                    candidate.is_hidden(),
                    candidate.last_pair(),
                )
            })
            .collect::<Vec<_>>();
        if let Some(record) = chosen
            && let Some(address) = record.address()
            && !accounts
                .iter()
                .any(|(operation, ..)| *operation == record.operation())
        {
            accounts.push((
                record.operation(),
                record.index(),
                address,
                record.is_hidden(),
                record
                    .swap()
                    .map(|swap| (swap.terms().sell_token(), swap.terms().buy_token())),
            ));
        }
        accounts.sort_by_key(|(_, index, ..)| std::cmp::Reverse(*index));
        let mut items = vec![SwapAccountSelectItem {
            operation: None,
            address: None,
            label: "New account (recommended)".into(),
        }];
        items.extend(
            accounts
                .into_iter()
                .map(|(operation, index, address, hidden, last_pair)| {
                    let mut label = vec![format!("#{index}"), railgun_ui::short_address(&address)];
                    if hidden {
                        label.push("Hidden".into());
                    }
                    if let Some((sell, buy)) = last_pair {
                        label.push(format!(
                            "Last used {} → {}",
                            self.token_symbol(sell, cx),
                            self.token_symbol(buy, cx)
                        ));
                    }
                    SwapAccountSelectItem {
                        operation: Some(operation),
                        address: Some(address),
                        label: label.join(" · ").into(),
                    }
                }),
        );
        items
    }

    /// Use a set-up stealth account for this new swap, or go back to a new account. The
    /// tokens, amount and slippage stay, and the swap is quoted again using recorded account
    /// state. Execution preparation checks the chosen account before proving.
    fn select_form_account(
        &mut self,
        operation: Option<ExecutorOperationId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let busy = self.busy();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(select) = form.account_select.clone() else {
            return;
        };
        if !busy && form.operation != operation {
            if let Some(operation) = operation {
                self.tracking.entry(operation).or_default().auto_place = false;
            }
            form.operation = operation;
            form.reuse_account = operation.is_some();
            form.error = None;
            // Each account quotes on its own orderbook route, which keeps the accounts
            // unlinked there.
            form.orderbook = None;
            form.route.invalidate_estimate();
        }
        let current = form.operation;
        if select.read(cx).selected_value() != Some(&current) {
            select.update(cx, |select, cx| {
                select.set_selected_value(&current, window, cx);
            });
        }
        // Load the chosen account's record, which may not belong to a swap, and drop a
        // previous one.
        self.reload_records();
        self.refresh_setup_route(cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// Continue or retry a swap. A retired setup starts a new review with a fresh account.
    pub(super) fn open_existing_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let Some(record) = self.record(operation) else {
            return;
        };
        if self.setup_retry_problem(record).is_some() {
            return;
        }
        if let Some(pending) = self.pending_order(record) {
            self.open_form(
                Some(operation),
                pending.sell,
                Some(pending.buy),
                Some(pending.amount),
                Some(pending.slippage_bps),
                window,
                cx,
            );
            if let Some(form) = self.form.as_mut() {
                form.back_to_detail = Some(operation);
                form.reuse_account = pending.reuse_account;
            }
            self.schedule_quote(window, cx);
            return;
        }
        let Some((sell, buy)) = swap_tokens(record) else {
            return;
        };
        let tracking = self.tracking.get(&operation);
        let amount =
            swap_sell_amount(record).or_else(|| tracking.and_then(|tracking| tracking.amount));
        let slippage = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .map(|order| order.bounds().slippage_bps)
            .or_else(|| {
                record
                    .swap_approval()
                    .map(|approval| approval.bounds.slippage_bps)
            })
            .or_else(|| tracking.and_then(|tracking| tracking.slippage_bps));
        let existing = (self.stage(record) != SwapStage::SetupRetired).then_some(operation);
        self.open_form(existing, sell, Some(buy), amount, slippage, window, cx);
        if let Some(form) = self.form.as_mut() {
            form.back_to_detail = Some(operation);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_form(
        &mut self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        buy: Option<Address>,
        amount: Option<U256>,
        slippage_bps: Option<u32>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let sell_items = private_action_asset_select_items(&self.sell_assets(operation, cx));
        let sell_index = select_index(&sell_items, sell);
        let buy_items = self.buy_items(sell, cx);
        let buy_index = buy.and_then(|buy| select_index(&buy_items, buy));
        let decimals = self.token_decimals(sell, cx);
        let sell_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(sell_items), sell_index, window, cx)
                .searchable(true)
        });
        let buy_select = cx.new(|cx| {
            ComboboxState::new(
                SearchableVec::new(buy_items),
                buy_index.into_iter().collect(),
                window,
                cx,
            )
            .searchable(true)
        });
        let amount_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("0.0");
            if let Some(amount) = amount {
                input.set_value(format_unshield_amount_input(amount, decimals), window, cx);
            }
            input
        });
        let account_select = operation.is_none().then(|| {
            let items = self.swap_account_items(None, cx);
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(items),
                    Some(gpui_component::IndexPath::default()),
                    window,
                    cx,
                )
                .searchable(true)
            })
        });
        let mut subscriptions = vec![
            cx.subscribe_in(
                &amount_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change)
                        && this
                            .form
                            .as_ref()
                            .is_some_and(|form| form.amount_input == *input)
                    {
                        this.form_inputs_changed(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &sell_select,
                window,
                |this,
                 _,
                 event: &SelectEvent<SearchableVec<PrivateActionAssetSelectItem>>,
                 window,
                 cx| {
                    if let SelectEvent::Confirm(Some(token)) = event {
                        this.set_form_sell(*token, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &buy_select,
                window,
                |this,
                 _,
                 event: &ComboboxEvent<SearchableVec<PrivateActionAssetSelectItem>>,
                 window,
                 cx| {
                    if let ComboboxEvent::Confirm(tokens) = event
                        && let Some(token) = tokens.first()
                    {
                        this.set_form_buy(*token, window, cx);
                    }
                },
            ),
        ];
        if let Some(select) = &account_select {
            subscriptions.push(cx.subscribe_in(
                select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<SwapAccountSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(operation)) = event {
                        this.select_form_account(*operation, window, cx);
                    }
                },
            ));
        }
        self.form = Some(SwapForm {
            operation,
            reuse_account: false,
            account_select,
            sell,
            buy,
            sell_select,
            buy_select,
            amount_input,
            slippage_bps: slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
            route: SetupRoute::default(),
            quote: QuoteState::Idle,
            quote_task: None,
            quote_revision: 0,
            orderbook: None,
            price_acknowledged: false,
            high_costs_acknowledged: false,
            error: None,
            back_to_detail: None,
            details_open: false,
            settings_open: false,
            receive_help_open: false,
            _subscriptions: subscriptions,
        });
        self.start_setup_route_updates(cx);
        self.schedule_quote(window, cx);
        self.show_view(SwapDialogView::Form, window, cx);
    }

    fn form_mode(&self, form: &SwapForm) -> FormMode {
        if form.reuse_account {
            return FormMode::Order;
        }
        swap_form_mode(
            form.operation
                .and_then(|operation| self.record(operation))
                .map(|record| self.stage(record)),
        )
    }

    pub(super) fn sell_assets(
        &self,
        operation: Option<ExecutorOperationId>,
        cx: &App,
    ) -> Vec<UnshieldAsset> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(self.session.chain_id)
            .and_then(EffectiveChainConfig::swap_profile)
        else {
            return Vec::new();
        };
        root.private_action_asset_options(DeliveryFormKind::Unshield, self.session.chain_id)
            .into_iter()
            .filter(|asset| {
                profile.token_eligibility(asset.token) == SwapTokenEligibility::Eligible
            })
            .map(|mut asset| {
                asset.max_batched = self
                    .owner
                    .max_swap_amount(&self.session, operation, asset.token)
                    .unwrap_or_default();
                asset
            })
            .collect()
    }

    /// The v1 destination list: configured tokens the swap profile accepts.
    fn buy_items(&self, sell: Address, cx: &App) -> Vec<PrivateActionAssetSelectItem> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(self.session.chain_id)
            .and_then(EffectiveChainConfig::swap_profile)
        else {
            return Vec::new();
        };
        let registry = &root.effective_token_registry;
        let mut items = swap_destination_tokens(registry, &profile)
            .into_iter()
            .filter_map(|info| {
                let token = info.token_address.parse::<Address>().ok()?;
                (token != sell).then(|| PrivateActionAssetSelectItem {
                    token,
                    label: Arc::from(info.symbol.as_str()),
                    icon_path: token_display_metadata(
                        Some(registry),
                        self.session.chain_id,
                        &token,
                    )
                    .and_then(|metadata| metadata.icon_path),
                })
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.label.cmp(&right.label));
        items
    }

    fn token_decimals(&self, token: Address, cx: &App) -> Option<u8> {
        let root = self.root.upgrade()?;
        token_display_metadata(
            Some(&root.read(cx).effective_token_registry),
            self.session.chain_id,
            &token,
        )
        .map(|metadata| metadata.decimals)
    }

    fn form_amount(&self, form: &SwapForm, cx: &App) -> Result<U256, String> {
        let amount = wallet_ops::parse_unshield_amount(
            &form.amount_input.read(cx).value(),
            self.token_decimals(form.sell, cx),
        )
        .map_err(|error| format!("{error:#}"))?;
        if amount.is_zero() {
            return Err("Enter an amount to swap.".into());
        }
        Ok(amount)
    }

    fn form_inputs_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.error = None;
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    fn set_form_sell(&mut self, token: Address, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.form.as_ref().is_none_or(|form| {
            (form.operation.is_some() && !form.reuse_account) || form.sell == token
        }) {
            return;
        }
        let items = self.buy_items(token, cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.sell = token;
        form.error = None;
        if form.buy == Some(token) {
            form.buy = None;
        }
        let selected = form.buy.and_then(|buy| select_index(&items, buy));
        form.buy_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            select.set_selected_indices(selected, window, cx);
        });
        form.route.invalidate_estimate();
        self.refresh_setup_route(cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    fn set_form_buy(&mut self, token: Address, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if (form.operation.is_some() && !form.reuse_account) || form.buy == Some(token) {
            return;
        }
        form.buy = Some(token);
        form.error = None;
        self.schedule_quote(window, cx);
        cx.notify();
    }

    fn set_slippage(&mut self, bps: u32, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.slippage_bps == bps {
            return;
        }
        form.slippage_bps = bps;
        self.schedule_quote(window, cx);
        // The popover lives in the quote details, which only a ready quote shows. Move focus
        // to the amount before they go, so keyboard input and Escape still reach the dialog.
        if self
            .form
            .as_ref()
            .is_some_and(|form| !matches!(form.quote, QuoteState::Ready(_)))
        {
            self.focus_form_amount(window, cx);
        }
        cx.notify();
    }

    fn use_amount(&mut self, amount: U256, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let value = format_unshield_amount_input(amount, self.token_decimals(form.sell, cx));
        let input = form.amount_input.clone();
        input.update(cx, |input, cx| input.set_value(value, window, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        // Programmatic input changes don't emit InputEvent::Change.
        self.form_inputs_changed(window, cx);
    }

    fn form_primary(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_setup_settings(cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        match self.form_mode(form) {
            FormMode::Setup { resume } => self.request_setup(resume, window, cx),
            FormMode::Order => {
                // A change still waiting for review is named when the user opens it.
                let change = self
                    .reapproval
                    .filter(|(pending, _)| form.operation == Some(*pending))
                    .map(|(_, change)| change);
                self.request_order_review(change, window, cx);
            }
            FormMode::SettingUp | FormMode::Placed => {}
        }
    }

    /// Resume a quote that waited for the setup's confirmation.
    pub(super) fn continue_form_after_observation(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.form_mode(form) == FormMode::Order
            && (matches!(form.quote, QuoteState::Idle | QuoteState::Failed(_))
                || matches!(&form.quote, QuoteState::Ready(review)
                    if review.plan().swap_executor().requires_setup()))
            && form.quote_task.is_none()
        {
            self.schedule_quote(window, cx);
        }
    }

    // Setup route

    fn start_setup_route_updates(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(root) = self.root.upgrade() {
            root.update(cx, |root, _| root.public_broadcaster_anchor_refresh.wake());
        }
        self.refresh_setup_route(cx);
        let task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL)
                    .await;
                let active = view
                    .update(cx, |view, cx| {
                        if view.form.is_none() || !view.session_is_current(cx) {
                            return false;
                        }
                        view.refresh_setup_route(cx);
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        });
        if let Some(form) = self.form.as_mut() {
            form.route.refresh_task = Some(task);
        }
    }

    pub(super) fn setup_fee_route(
        &self,
        preferred: Address,
        current: Option<Address>,
        allow_out_of_range: bool,
        favorites_only: bool,
        cx: &App,
    ) -> (
        Vec<PublicBroadcasterFeeTokenOption>,
        Option<Address>,
        Vec<PublicBroadcasterCandidate>,
    ) {
        let Some(root_entity) = self.root.upgrade() else {
            return (Vec::new(), None, Vec::new());
        };
        let root = root_entity.read(cx);
        let chain_id = self.session.chain_id;
        let Some(profile) = root
            .effective_chain_configs
            .get(chain_id)
            .and_then(EffectiveChainConfig::accepted_executor_profile)
        else {
            return (Vec::new(), None, Vec::new());
        };
        let policy = root.public_broadcaster_fee_policy(allow_out_of_range);
        let trust = root.public_broadcaster_trust_filter(favorites_only);
        let rows = root.monitor_fee_rows();
        let options = root
            .chain_states
            .get(&chain_id)
            .and_then(|state| state.snapshot())
            .map(|snapshot| {
                public_broadcaster_fee_token_options_from_snapshot(
                    snapshot,
                    &rows,
                    None,
                    Some(profile),
                    policy,
                    &trust,
                    Some(&root.effective_token_registry),
                    |token| {
                        root.public_broadcaster_anchor_cache
                            .cached_rate(chain_id, token)
                    },
                )
            })
            .unwrap_or_default();
        let token = (!options.is_empty()).then(|| {
            resolve_selected_public_broadcaster_fee_token(
                current.unwrap_or(preferred),
                preferred,
                &options,
            )
        });
        let candidates = token
            .map(|token| self.broadcaster_candidates(token, allow_out_of_range, favorites_only, cx))
            .unwrap_or_default();
        (options, token, candidates)
    }

    fn refresh_setup_route(&mut self, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if !matches!(self.form_mode(form), FormMode::Setup { .. }) || self.busy() {
            return;
        }
        let (options, token, candidates) = self.setup_fee_route(
            form.sell,
            form.route.fee_token,
            form.route.allow_out_of_range,
            form.route.favorites_only,
            cx,
        );
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let route = &mut form.route;
        let quote_changed = route.estimate_candidate.as_ref().is_some_and(|quoted| {
            !candidates
                .iter()
                .any(|candidate| same_offer(quoted, candidate))
        });
        let token_changed = route.fee_token != token;
        let choice_changed = route.selected.as_ref().is_some_and(|address| {
            !candidates
                .iter()
                .any(|candidate| &candidate.railgun_address == address)
        });
        if choice_changed {
            route.selected = None;
        }
        route.fee_options = options;
        route.fee_token = token;
        route.candidates = candidates;
        if quote_changed || token_changed || choice_changed {
            route.invalidate_estimate();
        }
        let due = route.estimate_task.is_none()
            && (route.estimate.is_none() && route.estimate_error.is_none()
                || route
                    .next_estimate
                    .is_some_and(|next| Instant::now() >= next));
        if due {
            self.schedule_setup_estimate(cx);
        }
        cx.notify();
    }

    fn schedule_setup_estimate(&mut self, cx: &mut Context<'_, Self>) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.route.candidates.is_empty() {
            return;
        }
        let selection =
            form.route
                .selected
                .as_ref()
                .map_or(PublicBroadcasterSelection::Random, |address| {
                    PublicBroadcasterSelection::Specific {
                        railgun_address: address.clone(),
                    }
                });
        let candidate = {
            let root = root.read(cx);
            wallet_ops::select_public_broadcaster_with_policy_and_trust(
                &form.route.candidates,
                &selection,
                root.public_broadcaster_fee_policy(form.route.allow_out_of_range),
                &root.public_broadcaster_trust_filter(form.route.favorites_only),
            )
        };
        let Ok(candidate) = candidate else {
            return;
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        let runtime = self.runtime.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let operation = form.operation;
        let route = &mut form.route;
        route.estimate_revision = route.estimate_revision.wrapping_add(1);
        let revision = route.estimate_revision;
        route.estimate_candidate = Some(candidate.clone());
        route.estimate = None;
        route.estimate_error = None;
        route.estimate_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(COST_ESTIMATE_DEBOUNCE).await;
            let result = runtime
                .spawn(async move {
                    Box::pin(owner.estimate_swap_setup_fee(&session, operation, candidate)).await
                })
                .await;
            let result = result
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = view.update(cx, |view, cx| {
                let Some(form) = view.form.as_mut() else {
                    return;
                };
                let route = &mut form.route;
                if route.estimate_revision != revision {
                    return;
                }
                route.estimate_task = None;
                route.next_estimate = Some(
                    Instant::now()
                        + if result.is_ok() {
                            Duration::from_secs(30)
                        } else {
                            Duration::from_secs(5)
                        },
                );
                match result {
                    Ok(estimate) => route.estimate = Some(estimate),
                    Err(error) => route.estimate_error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The broadcaster picker's view of the setup route.
    pub(in crate::root) fn setup_picker_context(&self) -> Option<RecoveryPickerContext> {
        let form = self.form.as_ref()?;
        Some(RecoveryPickerContext {
            chain_id: self.session.chain_id,
            token: form.route.fee_token?,
            choice: form.route.choice(),
            candidates: form.route.candidates.clone(),
            allow_out_of_range: form.route.allow_out_of_range,
            favorites_only: form.route.favorites_only,
            busy: self.busy(),
            estimating: form.route.estimate_task.is_some(),
            fee_context: form
                .route
                .estimate
                .as_ref()
                .map(BroadcasterPickerFeeEstimateContext::from),
        })
    }

    pub(in crate::root) fn set_setup_allow_out_of_range(
        &mut self,
        checked: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.route.allow_out_of_range = checked;
        form.route.invalidate_estimate();
        self.refresh_setup_route(cx);
    }

    pub(in crate::root) fn choose_setup_broadcaster(
        &mut self,
        address: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        self.refresh_setup_route(cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if !form
            .route
            .candidates
            .iter()
            .any(|candidate| candidate.railgun_address == address)
        {
            return;
        }
        form.route.selected = Some(address);
        form.route.invalidate_estimate();
        let input = form.amount_input.clone();
        self.schedule_setup_estimate(cx);
        input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    // Review, setup and submission

    fn request_setup(&mut self, resume: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.busy() {
            return;
        }
        let waku = self.broadcaster_network(cx);
        let (approval, review) = match self.setup_approval(resume, waku) {
            Ok(approved) => approved,
            Err(error) => {
                if let Some(form) = self.form.as_mut() {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let summary = self
            .swap_summary(&review, Some(&approval), None, cx)
            .requiring_explicit_review();
        self.request_authorization(SwapAction::Setup(Box::new(approval)), summary, window, cx);
    }

    /// The swap as reviewed: the quoted terms with the suggested minimum, and the setup's
    /// broadcaster route.
    fn setup_approval(
        &self,
        resume: bool,
        waku: Option<Arc<WakuDeliveryClient>>,
    ) -> Result<(SetupApproval, Arc<SwapReview>), String> {
        let form = self.form.as_ref().ok_or("The swap form closed.")?;
        let buy = form.buy.ok_or("Choose the token to receive.")?;
        let review = match &form.quote {
            QuoteState::Ready(review)
                if review.plan().sell_token() == form.sell && review.plan().buy_token() == buy =>
            {
                Arc::clone(review)
            }
            _ => return Err("Wait for the quote.".into()),
        };
        if let Some(problem) = form.review_problem(&review) {
            return Err(problem.into());
        }
        let estimate = form
            .route
            .estimate
            .as_ref()
            .ok_or("Wait for the setup fee estimate.")?;
        let candidate = estimate.broadcaster();
        if !form
            .route
            .candidates
            .iter()
            .any(|current| same_offer(current, candidate))
        {
            return Err("The broadcaster quote changed. Wait for a new estimate.".into());
        }
        let waku = waku.ok_or("Wait for the broadcaster network connection, then try again.")?;
        let approval = review
            .approval(review.suggested_private_minimum(), form.price_acknowledged)
            .map_err(|error| format!("{error:#}"))?;
        let operation = match form.operation {
            Some(operation) => operation,
            None => ExecutorOperationId::random().map_err(|error| error.to_string())?,
        };
        Ok((
            SetupApproval {
                operation,
                resume,
                sell: form.sell,
                buy,
                candidate: candidate.clone(),
                maximum_private_fee: default_public_broadcaster_fee_limit(estimate.fee_amount()),
                waku,
                approval,
                orderbook: form.orderbook.clone(),
            },
            review,
        ))
    }

    /// Reserve the stealth account, check that the approved amount still fits one order,
    /// persist the approval, and hand the setup to the broadcaster. Nothing is paid when the
    /// amount no longer fits. The order follows once the setup is confirmed.
    pub(super) fn submit_setup(
        &mut self,
        approval: SetupApproval,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let operation = approval.operation;
        let (progress, receiver) =
            tokio::sync::watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        let mut changes = receiver.clone();
        let watch = cx.spawn(async move |view, cx| {
            while changes.changed().await.is_ok() {
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let bounds = &approval.approval.bounds;
        let tracking = self.tracking.entry(operation).or_default();
        tracking.amount = Some(bounds.spend_amount());
        tracking.slippage_bps = Some(bounds.slippage_bps);
        if approval.orderbook.is_some() {
            tracking.orderbook.clone_from(&approval.orderbook);
        }
        tracking.setup = None;
        tracking.cursor = None;
        tracking.setup_read_at = None;
        tracking.located_at = None;
        tracking.error = None;
        tracking.setup_stage = Some(receiver);
        tracking.setup_watch = Some(watch);
        // The order was approved with the setup: place it as soon as the setup is confirmed.
        tracking.auto_place = true;
        let byte_budget = tracking.byte_budget;
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.start_job(
            operation,
            SwapJobKind::Setup,
            async move {
                let prepared = if approval.resume {
                    Box::pin(owner.resume_swap_setup(operation, approval.candidate, &authorization))
                        .await?
                } else {
                    Box::pin(owner.prepare_swap_setup(
                        operation,
                        approval.candidate,
                        approval.sell,
                        approval.buy,
                        &authorization,
                    ))
                    .await?
                };
                let reserved = SwapExecutor::reserved(&prepared)?;
                let request = SwapAmountRequest {
                    sell_token: approval.sell,
                    buy_token: approval.buy,
                    amount: approval.approval.bounds.spend_amount(),
                    byte_budget,
                };
                let planning_owner = Arc::clone(&owner);
                let planning_session = Arc::clone(&session);
                let plan = tokio::task::spawn_blocking(move || {
                    planning_owner.plan_swap_amount(&reserved, &planning_session, &request)
                })
                .await
                .map_err(|_| eyre::eyre!("Planning the swap stopped unexpectedly."))??;
                if matches!(plan, SwapAmountPlan::TooLarge { .. }) {
                    return Ok(SetupOutcome::TooLarge);
                }
                owner.record_swap_approval(operation, approval.approval)?;
                let outcome = Box::pin(owner.submit_swap_setup(
                    &prepared,
                    SwapSetupRequest {
                        maximum_private_fee: approval.maximum_private_fee,
                        session,
                        authorization,
                        waku: approval.waku,
                        verify_proof: true,
                        progress_tx: Some(progress),
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                    },
                ))
                .await?;
                Ok(SetupOutcome::Sent(outcome.result))
            },
            move |this, outcome, window, cx| {
                let tracking = this.tracking.entry(operation).or_default();
                tracking.setup_stage = None;
                tracking.setup_watch = None;
                match outcome {
                    SetupOutcome::Sent(result) => {
                        tracking.error = broadcaster_result_problem(&result, "setup");
                    }
                    SetupOutcome::TooLarge => {
                        tracking.auto_place = false;
                        // The form offers the largest amount that fits, for a new review.
                        if !window.has_active_dialog(cx)
                            || this.detail_is_active(operation, window, cx)
                        {
                            this.open_existing_form(operation, window, cx);
                        }
                        this.fail(operation, "Your notes changed after the quote, and the approved amount no longer fits one swap. Nothing was paid.".into());
                    }
                }
                cx.notify();
            },
            window,
            cx,
        );
        // The detail follows the setup and then the order it places.
        self.show_detail(operation, window, cx);
    }

    // Quote and order

    fn retry_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // An explicit retry gets a new isolated route instead of reusing a failed circuit.
        form.orderbook = None;
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.orderbook = None;
        }
        self.schedule_quote(window, cx);
    }

    fn schedule_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let operation = form.operation;
        let mode = self.form_mode(form);
        if !matches!(mode, FormMode::Setup { .. } | FormMode::Order) {
            return;
        }
        let (sell, buy, slippage_bps) = (form.sell, form.buy, form.slippage_bps);
        let amount = self.form_amount(form, cx).ok();
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        });
        let (Some(buy), Some(amount), Some((anchor_cache, tokens))) = (buy, amount, registries)
        else {
            if let Some(form) = self.form.as_mut() {
                form.quote = QuoteState::Idle;
                form.quote_task = None;
                form.receive_help_open = false;
            }
            cx.notify();
            return;
        };
        // Before setup the quote uses a stand-in. Set-up accounts use local observations;
        // execution preparation refreshes the account before proving and signing.
        let executor = match (mode, operation) {
            (FormMode::Setup { resume: true }, Some(operation)) => QuoteExecutor::Setup(operation),
            (FormMode::Order, Some(operation)) => QuoteExecutor::Order {
                operation,
                reuse: form.reuse_account,
            },
            _ => QuoteExecutor::Preview,
        };
        let tracking = operation.map(|operation| self.tracking.entry(operation).or_default());
        let byte_budget = tracking.as_ref().and_then(|tracking| tracking.byte_budget);
        let tracked_orderbook = tracking.and_then(|tracking| {
            tracking.amount = Some(amount);
            tracking.slippage_bps = Some(slippage_bps);
            tracking.orderbook.clone()
        });
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let request = QuoteRequest {
            executor,
            sell,
            buy,
            amount,
            slippage_bps,
            byte_budget,
            orderbook: form.orderbook.clone().or(tracked_orderbook),
            // A fresh signing-time failure must reach the acknowledgement even if the
            // background cache still contains an older rate.
            anchor_cache: if operation.is_some_and(|operation| {
                self.reapproval == Some((operation, SwapReviewChange::PriceUnavailable))
            }) {
                None
            } else {
                Some(anchor_cache)
            },
            tokens,
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        let runtime = self.runtime.clone();
        form.quote_revision = form.quote_revision.wrapping_add(1);
        let revision = form.quote_revision;
        form.quote = QuoteState::Loading;
        form.receive_help_open = false;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let result = runtime.spawn(quote_swap(owner, session, request)).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_quote(operation, revision, result.ok(), window, cx);
            });
        }));
        cx.notify();
    }

    fn apply_quote(
        &mut self,
        operation: Option<ExecutorOperationId>,
        revision: u64,
        result: Option<QuoteResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.operation != operation || form.quote_revision != revision {
            return;
        }
        form.quote_task = None;
        form.high_costs_acknowledged = false;
        let Some(result) = result else {
            form.quote =
                QuoteState::Failed(eyre::eyre!("Quoting stopped unexpectedly. Try again."));
            cx.notify();
            return;
        };
        if let Some(orderbook) = result.orderbook {
            form.orderbook = Some(orderbook.clone());
            if let Some(operation) = operation {
                self.tracking.entry(operation).or_default().orderbook = Some(orderbook);
            }
        }
        form.quote = match result.outcome {
            Ok(QuoteOutcome::TooLarge(largest)) => QuoteState::TooLarge { largest },
            Ok(QuoteOutcome::Review(review)) => {
                if *review.price() != SwapPrice::Unverified {
                    form.price_acknowledged = false;
                }
                QuoteState::Ready(Arc::from(review))
            }
            Ok(QuoteOutcome::PriceBlocked(block)) => QuoteState::PriceBlocked(block),
            Err(error) => QuoteState::Failed(error),
        };
        // Only the swap's own form, in front, reopens the review. Otherwise the change stays
        // pending, and the form's Review… names it.
        let reopen = matches!(&form.quote, QuoteState::Ready(review)
            if form.review_problem(review).is_none())
            && operation.is_some_and(|operation| {
                self.reapproval
                    .is_some_and(|(pending, _)| pending == operation)
                    && self.swap_dialog_shows(operation, window, cx)
            });
        // Setup can confirm while its retry quote is in flight. Replace that setup preview
        // with an order quote before offering approval.
        if self.form.as_ref().is_some_and(|form| {
            self.form_mode(form) == FormMode::Order
                && matches!(&form.quote, QuoteState::Ready(review)
                    if review.plan().swap_executor().requires_setup())
        }) {
            self.schedule_quote(window, cx);
            return;
        }
        if reopen && let Some((_, change)) = self.reapproval {
            self.request_order_review(Some(change), window, cx);
        }
        cx.notify();
    }

    fn request_order_review(
        &mut self,
        change: Option<SwapReviewChange>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(operation) = form.operation else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let orderbook = form.orderbook.clone().or_else(|| {
            self.tracking
                .get(&operation)
                .and_then(|tracking| tracking.orderbook.clone())
        });
        let problem = form.review_problem(review).or_else(|| {
            orderbook
                .is_none()
                .then_some("The swap's orderbook route isn't ready. Refresh the quote.")
        });
        let (Some(orderbook), None) = (orderbook, problem) else {
            if let Some(form) = self.form.as_mut() {
                form.error = problem.map(Into::into);
            }
            cx.notify();
            return;
        };
        let approval = OrderApproval {
            operation,
            review: Arc::clone(review),
            private_minimum: review.suggested_private_minimum(),
            price_acknowledged: form.price_acknowledged,
            orderbook,
        };
        let summary = self
            .swap_summary(&approval.review, None, change, cx)
            .requiring_explicit_review();
        // The review about to open names the pending change.
        if self
            .reapproval
            .is_some_and(|(pending, _)| pending == operation)
        {
            self.reapproval = None;
        }
        self.request_authorization(SwapAction::Order(approval), summary, window, cx);
    }

    /// The single review of a swap. With `setup`, a new swap's setup and its order, placed once
    /// the setup is confirmed; without, an order for a stealth account that is set up.
    fn swap_summary(
        &self,
        review: &SwapReview,
        setup: Option<&SetupApproval>,
        change: Option<SwapReviewChange>,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let sell_amount = self.token_amount(sell, plan.amount(), cx);
        let buy_symbol = self.token_symbol(buy, cx);
        let valid_for = self
            .swap_profile(cx)
            .map_or(10, |profile| profile.valid_to_window().as_secs() / 60);
        let slippage = format_bps_percent(u64::from(review.slippage_bps()));
        let mut rows = Vec::new();
        if let Some(setup) = setup {
            let root = self.root.upgrade();
            let maximum_fee = format_token_amount_ceiling_for_display(
                self.session.chain_id,
                setup.candidate.token,
                setup.maximum_private_fee,
                root.as_ref()
                    .map(|root| &root.read(cx).effective_token_registry),
            );
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Pay now",
                    format!("Up to {maximum_fee} setup fee"),
                )
                .with_icon(self.token_icon(setup.candidate.token, cx))
                .with_note(format!(
                    "Via broadcaster {}. Not refunded if the order doesn't fill.",
                    broadcaster_candidate_label(&setup.candidate)
                )),
            );
        }
        rows.push(
            SpendAuthorizationSummaryRow::new(
                "You receive",
                format!("≈ {}", self.token_amount(buy, expected_output(review), cx)),
            )
            .with_icon(self.token_icon(buy, cx))
            .with_note(format!(
                "At least {}",
                self.token_amount(buy, review.suggested_private_minimum(), cx)
            )),
        );
        if plan.swap_executor().is_reused() {
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Stealth account",
                    plan.executor().to_checksum(None),
                )
                .with_shortened_copyable(),
            );
        }
        let isolated = review.isolation() == OperationNetworkIsolation::Dedicated;
        if let OperationNetworkIsolation::Unavailable(mode) = review.isolation() {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Network route",
                format!("Not isolated in {mode} mode"),
            ));
        }
        let details = vec![
            ("Slippage", slippage.clone()),
            (
                "CoW network fee",
                format!(
                    "{}, in the quote",
                    self.token_amount(buy, cow_fee(review), cx)
                ),
            ),
            (
                "Hook gas",
                format!(
                    "Up to {}, covered by the minimum",
                    self.token_amount(buy, review.hook_cost(), cx)
                ),
            ),
            ("Railgun fees", railgun_fees_label(review)),
            (
                "Order valid for",
                if setup.is_some() {
                    format!("{valid_for} minutes after setup")
                } else {
                    format!("{valid_for} minutes")
                },
            ),
        ];
        let mut context = "Placing the order publishes its tokens, amounts, price limit, and hook data, including the notes it spends, even if it never fills. A later spend of those notes can be linked to this swap.".to_owned();
        if !isolated {
            context.push_str(
                "\nThis network mode can't give the swap its own network route, so its orderbook requests aren't isolated from your other wallet traffic.",
            );
        }
        let (title, confirm_label) = if setup.is_some() {
            ("Set up stealth account and swap", "Create stealth account")
        } else {
            ("Private swap", "Swap")
        };
        let summary = SpendAuthorizationSummary::new(title, "", rows)
            .with_title_chip(self.chain_label())
            .with_asset_pair(
                SpendAuthorizationAsset::new(sell_amount, self.token_icon(sell, cx)),
                SpendAuthorizationAsset::new(buy_symbol, self.token_icon(buy, cx)),
            )
            .with_details(
                "Order terms",
                format!("{slippage} slippage · {valid_for} min"),
                details,
                Some(
                    "If someone triggers the swap's unshield and the order doesn't fill, recovering the tokens costs the unshield and shield fees.",
                ),
            )
            .with_info_context("What becomes public", context)
            .with_confirm_label(confirm_label);
        let summary = if setup.is_some() {
            summary
                .with_progress(
                    1,
                    2,
                    "Step 1 of 2 · you confirm the order once setup completes",
                )
                .with_once_lifetime_note("You'll enter the password again to place the order.")
        } else {
            summary
        };
        let mut warnings = Vec::new();
        if *review.price() == SwapPrice::Unverified {
            warnings.push(Arc::from(UNVERIFIED_PRICE_WARNING));
        }
        if let Some(bps) = high_cost_bps(review) {
            warnings.push(Arc::from(high_cost_message(bps)));
        }
        if plan.swap_executor().is_reused() {
            warnings.push(Arc::from(ACCOUNT_REUSE_NOTE));
        }
        if let Some(change) = change {
            warnings.push(Arc::from(format!(
                "The terms changed since your last review: {}. Check them before you approve.",
                review_change_label(change)
            )));
        }
        summary.with_warnings(warnings)
    }

    /// Place the order approved with the setup once the setup is confirmed. The approved amount
    /// is planned and quoted again first; unchanged terms need only a confirm-only step, which a
    /// remembered spend authorization satisfies without a prompt.
    pub(super) fn place_approved_order(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(record) = self.record(operation) else {
            return;
        };
        if self.stage(record) != SwapStage::Approved {
            return;
        }
        let (Some((sell, buy)), Some(approval)) = (swap_tokens(record), record.swap_approval())
        else {
            return;
        };
        let approval = approval.clone();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (anchor_cache, tokens) = {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        };
        let tracking = self.tracking.entry(operation).or_default();
        let request = QuoteRequest {
            executor: QuoteExecutor::Order {
                operation,
                reuse: false,
            },
            sell,
            buy,
            amount: approval.bounds.spend_amount(),
            slippage_bps: approval.bounds.slippage_bps,
            byte_budget: tracking.byte_budget,
            orderbook: tracking.orderbook.clone(),
            anchor_cache: Some(anchor_cache),
            tokens,
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.start_job(
            operation,
            SwapJobKind::Requote,
            async move { Ok::<_, eyre::Report>(quote_swap(owner, session, request).await) },
            move |this, result, window, cx| {
                this.apply_approved_quote(operation, &approval, result, window, cx);
            },
            window,
            cx,
        );
    }

    fn apply_approved_quote(
        &mut self,
        operation: ExecutorOperationId,
        approval: &SwapApproval,
        result: QuoteResult,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let tracking = self.tracking.entry(operation).or_default();
        if let Some(orderbook) = &result.orderbook {
            tracking.orderbook = Some(orderbook.clone());
        }
        // Whatever the answer, the next attempt waits for the user.
        tracking.auto_place = false;
        let outcome = match result.outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                self.fail(operation, format!("{error:#}"));
                return;
            }
        };
        // Keep the detail visible during the request. Only another dialog defers
        // authorization or review; failures remain available without an automatic retry.
        if window.has_active_dialog(cx) && !self.detail_is_active(operation, window, cx) {
            self.tracking.entry(operation).or_default().auto_place = true;
            return;
        }
        let change = match outcome {
            QuoteOutcome::Review(review) => match review.approval_change(approval) {
                None => {
                    let Some(orderbook) = result.orderbook else {
                        return;
                    };
                    let approval = OrderApproval {
                        operation,
                        review: Arc::from(review),
                        private_minimum: approval.bounds.private_minimum,
                        price_acknowledged: approval.price_acknowledged,
                        orderbook,
                    };
                    let summary = self.place_summary(&approval, cx);
                    self.request_authorization(SwapAction::Order(approval), summary, window, cx);
                    return;
                }
                Some(change) => Some(change),
            },
            QuoteOutcome::PriceBlocked(PriceBlock::Deviates) => {
                Some(SwapReviewChange::QuoteDeviates)
            }
            // The form shows the largest amount that fits.
            QuoteOutcome::TooLarge(_) => None,
        };
        // Nothing was signed. The form quotes the current terms and, once they are ready,
        // opens the review with the change named.
        self.reapproval = change.map(|change| (operation, change));
        self.open_existing_form(operation, window, cx);
    }

    /// The confirm-only step for an order approved with its setup, whose terms still hold.
    fn place_summary(&self, approval: &OrderApproval, cx: &App) -> SpendAuthorizationSummary {
        let plan = approval.review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let valid_for = self
            .swap_profile(cx)
            .map_or(10, |profile| profile.valid_to_window().as_secs() / 60);
        SpendAuthorizationSummary::new(
            "Place swap order",
            "Costs and minimum were checked again and still match what you approved.",
            vec![
                SpendAuthorizationSummaryRow::new(
                    "You receive",
                    format!(
                        "≈ {}",
                        self.token_amount(buy, expected_output(&approval.review), cx)
                    ),
                )
                .with_icon(self.token_icon(buy, cx))
                .with_note(format!(
                    "At least {}",
                    self.token_amount(buy, approval.private_minimum, cx)
                )),
            ],
        )
        .with_title_chip(self.chain_label())
        .with_progress(2, 2, "Step 2 of 2 · stealth account is set up")
        .with_asset_pair(
            SpendAuthorizationAsset::new(
                self.token_amount(sell, plan.amount(), cx),
                self.token_icon(sell, cx),
            ),
            SpendAuthorizationAsset::new(self.token_symbol(buy, cx), self.token_icon(buy, cx)),
        )
        .with_details(
            "Order terms",
            format!("{valid_for} min from now"),
            vec![
                (
                    "CoW network fee",
                    format!(
                        "{}, in the quote",
                        self.token_amount(buy, cow_fee(&approval.review), cx)
                    ),
                ),
                (
                    "Hook gas",
                    format!(
                        "Up to {}, covered by the minimum",
                        self.token_amount(buy, approval.review.hook_cost(), cx)
                    ),
                ),
                ("Order valid for", format!("{valid_for} minutes from now")),
            ],
            None,
        )
        .with_confirm_label("Place order")
        .with_warnings(if *approval.review.price() == SwapPrice::Unverified {
            vec![Arc::from(UNVERIFIED_PRICE_WARNING)]
        } else {
            Vec::new()
        })
    }

    pub(super) fn submit_order(
        &mut self,
        approval: OrderApproval,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (anchors, tokens) = {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        };
        let operation = approval.operation;
        let plan = approval.review.plan();
        let pending = super::PendingSwapOrder {
            previous_order: self
                .record(operation)
                .and_then(|record| record.swap())
                .and_then(|swap| swap.orders().last())
                .map(wallet_ops::vault::SwapOrderRecord::uid),
            sell: plan.sell_token(),
            buy: plan.buy_token(),
            amount: plan.amount(),
            private_minimum: approval.private_minimum,
            slippage_bps: approval.review.slippage_bps(),
            reuse_account: plan.swap_executor().is_reused(),
            started_at: super::now_unix(),
        };
        self.tracking.entry(operation).or_default().pending_order = Some(pending);
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.reapproval = None;
        self.start_job(
            operation,
            SwapJobKind::Order,
            async move {
                Box::pin(owner.submit_swap_order(SwapOrderRequest {
                    review: approval.review.as_ref(),
                    private_minimum: approval.private_minimum,
                    price_acknowledged: approval.price_acknowledged,
                    session,
                    authorization,
                    orderbook: &approval.orderbook,
                    anchor_cache: &anchors,
                    token_registry: &tokens,
                    verify_proof: true,
                }))
                .await
            },
            move |this, outcome, window, cx| this.finish_order(operation, outcome, window, cx),
            window,
            cx,
        );
    }

    pub(super) fn finish_order(
        &mut self,
        operation: ExecutorOperationId,
        outcome: SwapOrderOutcome,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let shown = self.swap_dialog_shows(operation, window, cx);
        if let SwapOrderOutcome::Submitted { .. } = outcome {
            let tracking = self.tracking.entry(operation).or_default();
            tracking.cursor = None;
            tracking.error = None;
            tracking.auto_place = false;
            // The swap's own form or detail moves on to the detail; another dialog stays.
            if !window.has_active_dialog(cx) || shown {
                self.show_detail(operation, window, cx);
            }
            cx.notify();
            return;
        }
        if window.has_active_dialog(cx)
            && !shown
            && self
                .form
                .as_ref()
                .is_none_or(|form| form.operation != Some(operation))
        {
            match outcome {
                SwapOrderOutcome::ReviewRequired(change) => {
                    self.reapproval = Some((operation, change));
                    self.fail(
                        operation,
                        format!("Review the swap again: {}.", review_change_label(change)),
                    );
                }
                SwapOrderOutcome::Replan { byte_budget, .. } => {
                    self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                    self.fail(operation, "The order is too large. Retry with a smaller amount after this attempt ends.".into());
                }
                SwapOrderOutcome::Submitted { .. } => {}
            }
            cx.notify();
            return;
        }
        // An order placed from the detail continues in the form.
        if self
            .form
            .as_ref()
            .is_none_or(|form| form.operation != Some(operation))
        {
            self.open_existing_form(operation, window, cx);
        }
        match outcome {
            SwapOrderOutcome::Submitted { .. } => {}
            SwapOrderOutcome::ReviewRequired(change) => {
                // Nothing was signed. Quote the current terms, then review them again.
                self.reapproval = Some((operation, change));
                self.schedule_quote(window, cx);
            }
            SwapOrderOutcome::Replan {
                byte_budget,
                attempt_recorded,
            } => {
                self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                if let Some(form) = self.form.as_mut() {
                    form.quote = QuoteState::Idle;
                    form.error = Some(if attempt_recorded {
                        "The orderbook rejected this order's size. The order stays recorded until it expires. Then retry, and the swap offers the largest amount that fits."
                    } else {
                        "The order is too large for the orderbook. Enter a smaller amount."
                    }
                    .into());
                }
                if !attempt_recorded {
                    self.schedule_quote(window, cx);
                }
            }
        }
        cx.notify();
    }

    // Presentation helpers

    pub(super) fn swap_profile(&self, cx: &App) -> Option<wallet_ops::settings::SwapProfile> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.session.chain_id)?
            .swap_profile()
    }

    fn chain_label(&self) -> String {
        railgun_ui::chain_name(self.session.chain_id)
            .map_or_else(|| self.session.chain_id.to_string(), str::to_owned)
    }

    pub(super) fn token_icon(
        &self,
        token: Address,
        cx: &App,
    ) -> Option<crate::assets::WalletIconSource> {
        let root = self.root.upgrade()?;
        token_display_metadata(
            Some(&root.read(cx).effective_token_registry),
            self.session.chain_id,
            &token,
        )
        .and_then(|metadata| metadata.icon_path)
    }

    /// "1 WETH = 2,601.30 USDC" at the quoted trading rate, excluding explicit fees.
    fn rate_label(&self, review: &SwapReview, cx: &App) -> String {
        let plan = review.plan();
        self.pair_rate_label(
            plan.sell_token(),
            plan.buy_token(),
            review.quote().sell_amount,
            review.quote().buy_amount,
            cx,
        )
        .unwrap_or_else(|| "Unavailable".into())
    }

    /// "1 WETH = 2,601.30 USDC" for `sell_amount` of `sell` exchanged for `buy_amount` of `buy`.
    pub(super) fn pair_rate_label(
        &self,
        sell: Address,
        buy: Address,
        sell_amount: U256,
        buy_amount: U256,
        cx: &App,
    ) -> Option<String> {
        let sell_decimals = self.token_decimals(sell, cx)?;
        let buy_decimals = self.token_decimals(buy, cx)?;
        if sell_amount.is_zero() {
            return None;
        }
        let rate = buy_amount.saturating_mul(U256::from(10u8).pow(U256::from(sell_decimals)))
            / sell_amount;
        Some(format!(
            "1 {} = {} {}",
            self.token_symbol(sell, cx),
            railgun_ui::format_token_amount(rate, buy_decimals),
            self.token_symbol(buy, cx)
        ))
    }

    // Rendering

    /// "Retry swap" once an attempt of this swap ended, otherwise "Swap".
    pub(super) fn form_title(&self) -> &'static str {
        let retry = self.form.as_ref().is_some_and(|form| {
            !form.reuse_account
                && form
                    .operation
                    .and_then(|operation| self.record(operation))
                    .and_then(ExecutorRecord::swap)
                    .is_some_and(|swap| !swap.orders().is_empty())
        });
        if retry { "Retry swap" } else { "Swap" }
    }

    /// The order detail the form returns to, when it continues or retries a swap.
    pub(super) fn form_back_target(&self) -> Option<ExecutorOperationId> {
        self.form.as_ref()?.back_to_detail
    }

    pub(super) fn focus_form_amount(&self, window: &mut Window, cx: &mut App) {
        if let Some(form) = self.form.as_ref() {
            form.amount_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
    }

    pub(super) fn close_setup_settings(&mut self, cx: &mut Context<'_, Self>) {
        self.set_setup_settings_open(false, cx);
    }

    fn set_setup_settings_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.settings_open != open
        {
            form.settings_open = open;
            cx.notify();
        }
    }

    fn set_receive_help_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.receive_help_open != open
        {
            form.receive_help_open = open;
            cx.notify();
        }
    }

    fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.details_open = !form.details_open;
            cx.notify();
        }
    }

    /// Swap the sell and buy tokens. The amount belonged to the old sell token, so it clears.
    fn flip_tokens(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let (sell, Some(buy)) = (form.sell, form.buy) else {
            return;
        };
        let (input, sell_select, buy_select) = (
            form.amount_input.clone(),
            form.sell_select.clone(),
            form.buy_select.clone(),
        );
        input.update(cx, |input, cx| input.set_value("", window, cx));
        sell_select.update(cx, |select, cx| {
            select.set_selected_value(&buy, window, cx);
        });
        self.set_form_sell(buy, window, cx);
        self.set_form_buy(sell, window, cx);
        buy_select.update(cx, |select, cx| {
            select.set_selected_values(&[sell], window, cx);
        });
    }

    /// Open the broadcaster picker on top of the swap dialog. The popover draws above dialogs,
    /// so it closes first, and focus moves to the amount, where the picker returns it.
    fn choose_specific_setup_broadcaster(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(token) = form.route.fee_token else {
            return;
        };
        form.settings_open = false;
        let input = form.amount_input.clone();
        input.read(cx).focus_handle(cx).focus(window, cx);
        let target = BroadcasterPickerTarget::Swap(cx.weak_entity());
        let chain_id = self.session.chain_id;
        let _ = self.root.update(cx, |root, cx| {
            root.open_broadcaster_picker_for_target(
                target,
                "Swap setup",
                chain_id,
                token,
                window,
                cx,
            );
        });
        cx.notify();
    }

    /// The wallet's cached USD value of `amount` in micro-dollars, for display only. The price
    /// check never uses it.
    pub(super) fn usd_micro_value(&self, token: Address, amount: U256, cx: &App) -> Option<U256> {
        let root = self.root.upgrade()?;
        root.read(cx)
            .public_broadcaster_anchor_cache
            .cached_token_usd_micro_value(self.session.chain_id, token, amount)
    }

    fn usd_label(&self, token: Address, amount: U256, cx: &App) -> Option<String> {
        let usd = self.usd_micro_value(token, amount, cx)?;
        Some(format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
    }

    /// `label`, which shows `amount` of `token`, followed by the amount's USD value as the
    /// wallet shows it elsewhere: "0.0025 WETH · $6.76", nothing for a dollar stablecoin, or
    /// "· USD unavailable" without a cached rate.
    pub(super) fn with_usd(&self, label: String, token: Address, amount: U256, cx: &App) -> String {
        format_value_with_usd_label(
            label,
            amount,
            self.token_decimals(token, cx),
            self.usd_micro_value(token, amount, cx),
            false,
        )
    }

    /// The USD value [`Self::with_usd`] would show for `amount` of `token`, "$6.76": none for a
    /// dollar stablecoin or without a cached rate.
    pub(super) fn usd_value(&self, token: Address, amount: U256, cx: &App) -> Option<String> {
        let usd = self.usd_micro_value(token, amount, cx)?;
        self.token_decimals(token, cx)
            .is_none_or(|decimals| {
                railgun_ui::non_redundant_usd_micro_value(amount, decimals, usd).is_some()
            })
            .then(|| railgun_ui::format_usd_micro_value(usd))
    }

    /// The private balance of `token`, when the wallet holds any.
    fn private_balance_label(&self, token: Address, cx: &App) -> Option<String> {
        let root = self.root.upgrade()?;
        let total = root
            .read(cx)
            .private_action_asset_options(DeliveryFormKind::Unshield, self.session.chain_id)
            .into_iter()
            .find(|asset| asset.token == token)?
            .total;
        Some(self.token_amount(token, total, cx))
    }

    /// An amount without its symbol, for a panel whose token select names the token.
    pub(super) fn bare_amount(&self, token: Address, amount: U256, cx: &App) -> String {
        self.token_decimals(token, cx).map_or_else(
            || amount.to_string(),
            |decimals| railgun_ui::format_token_amount(amount, decimals),
        )
    }

    /// "#181 · 0x7d20…aa10" for the swap's own stealth account.
    fn account_label(&self, operation: ExecutorOperationId) -> Option<String> {
        let record = self.record(operation)?;
        Some(record.address().map_or_else(
            || format!("#{}", record.index()),
            |address| {
                format!(
                    "#{} · {}",
                    record.index(),
                    railgun_ui::short_address(&address)
                )
            },
        ))
    }

    /// "Setup ≈ 0.0001 WETH · $0.28 · random broadcaster", or where the estimate stands.
    fn setup_line(&self, form: &SwapForm, cx: &App) -> String {
        if let Some(estimate) = &form.route.estimate {
            let broadcaster = if form.route.selected.is_some() {
                selected_broadcaster_label(&form.route.choice(), &form.route.candidates)
            } else {
                "random broadcaster".to_owned()
            };
            let (token, fee) = (estimate.broadcaster().token, estimate.fee_amount());
            return format!(
                "Setup ≈ {} · {broadcaster}",
                self.with_usd(self.token_amount(token, fee, cx), token, fee, cx)
            );
        }
        let status = if let Some(error) = &form.route.estimate_error {
            error.clone()
        } else if form.route.estimate_task.is_some() {
            "Estimating…".into()
        } else if form.route.fee_options.is_empty() {
            "No spendable private fee token".into()
        } else {
            "Waiting for a compatible broadcaster".into()
        };
        format!("Setup · {status}")
    }

    /// The form and its footer: the credit, Cancel, and the form's primary action.
    pub(super) fn render_form(&self, cx: &Context<'_, Self>) -> (gpui::Div, Option<gpui::Div>) {
        let Some(form) = self.form.as_ref() else {
            return (app_muted_text("Wallet session ended."), None);
        };
        let mode = self.form_mode(form);
        let quoting = matches!(mode, FormMode::Setup { .. } | FormMode::Order);
        let editable = quoting && !self.busy();
        let locked = form.operation.is_some() && !form.reuse_account;
        let sell_assets = self.sell_assets(form.operation, cx);
        let review = match &form.quote {
            QuoteState::Ready(review) if quoting => Some(review),
            _ => None,
        };
        let close_settings = cx.entity();
        let footer_close_settings = close_settings.clone();
        let body = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            // A press elsewhere in the form closes the setup broadcaster popover. The popover
            // and the fee token list it opens draw above the form, so presses there don't.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                close_settings.update(cx, Self::close_setup_settings);
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.render_sell_panel(form, &sell_assets, editable, locked, cx))
                    .child(
                        div()
                            .relative()
                            .child(self.render_buy_panel(form, editable, locked, cx))
                            .child(self.render_flip(form, &sell_assets, editable, locked, cx)),
                    ),
            )
            .children(self.render_price_acknowledgement(form, cx))
            .child(self.render_account_row(form, mode, editable, cx))
            .children(review.map(|review| {
                self.render_details(
                    form,
                    review,
                    matches!(mode, FormMode::Setup { .. }),
                    editable,
                    cx,
                )
            }))
            .children(form.error.as_ref().map(|error| {
                Alert::error("swap-form-error", error.clone())
                    .small()
                    .min_w_0()
            }));
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            // The footer sits outside the form, so it closes the popover the same way.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                footer_close_settings.update(cx, Self::close_setup_settings);
            })
            .child(powered_by_cow(cx))
            .child(div().flex_1())
            .child(
                app_button("swap-form-cancel", "Cancel")
                    .flex_none()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_swap_dialog(window, cx);
                    })),
            )
            .child(self.render_form_primary(form, mode, cx));
        (body, Some(footer))
    }

    fn render_form_primary(
        &self,
        form: &SwapForm,
        mode: FormMode,
        cx: &Context<'_, Self>,
    ) -> gpui::AnyElement {
        let busy = self.busy();
        let quoted = match &form.quote {
            QuoteState::Ready(review) => form.review_problem(review).is_none(),
            _ => false,
        };
        match mode {
            FormMode::Setup { .. } | FormMode::SettingUp | FormMode::Order => {
                // A new swap's review also approves its setup, so it needs the setup's fee.
                let ready = match mode {
                    FormMode::Setup { .. } => quoted && form.route.estimate.is_some(),
                    FormMode::Order => quoted,
                    FormMode::SettingUp | FormMode::Placed => false,
                };
                app_button("swap-form-review", "Review…")
                    .primary()
                    .flex_none()
                    .loading(busy)
                    .disabled(busy || !ready)
                    .on_click(cx.listener(|this, _, window, cx| this.form_primary(window, cx)))
                    .into_any_element()
            }
            FormMode::Placed => {
                let operation = form.operation;
                app_button("swap-form-progress", "View progress…")
                    .flex_none()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(operation) = operation {
                            this.navigate(SwapDialogView::Detail(operation), window, cx);
                        }
                    }))
                    .into_any_element()
            }
        }
    }

    /// The Sell panel: the amount, its token, what's available, and the amount-reduction
    /// prompt when the amount doesn't fit one swap.
    fn render_sell_panel(
        &self,
        form: &SwapForm,
        sell_assets: &[UnshieldAsset],
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let asset = sell_assets.iter().find(|asset| asset.token == form.sell);
        let available = asset.map_or(U256::ZERO, |asset| match &form.quote {
            QuoteState::TooLarge { largest } => (*largest).min(asset.max_batched),
            _ => asset.max_batched,
        });
        let available_label = self.token_amount(form.sell, available, cx);
        let locked_value = asset.map_or(U256::ZERO, |asset| {
            self.session.locked_note_value(asset.token)
        });
        let usd = self
            .form_amount(form, cx)
            .ok()
            .and_then(|amount| self.usd_label(form.sell, amount, cx));
        let too_large = self.render_too_large(form, editable, cx);
        let submit_view = cx.entity();
        let submit_enabled = editable && matches!(form.quote, QuoteState::Ready(_));
        amount_panel(too_large.is_some(), cx)
            .child(app_muted_text("Sell"))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_action(move |_: &InputEnter, window, cx| {
                                if submit_enabled {
                                    submit_view
                                        .update(cx, |view, cx| view.form_primary(window, cx));
                                }
                            })
                            .child(app_amount_input(&form.amount_input).disabled(!editable)),
                    )
                    .child(token_pill(ui::private_action::asset_select(
                        &form.sell_select,
                        locked || !editable,
                    ))),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .child(div().flex_1().min_w_0().children(usd.map(app_muted_text)))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .children(asset.map(|_| {
                                balance_text(if locked_value.is_zero() {
                                    available_label.clone()
                                } else {
                                    format!(
                                        "{available_label} · {} locked",
                                        self.token_amount(form.sell, locked_value, cx)
                                    )
                                })
                            }))
                            .when(!locked_value.is_zero(), |row| {
                                row.child(
                                    app_button_base("swap-review-locked-notes")
                                        .label("Review")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                                        .tooltip("Show the locked notes and what holds them")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_locked_notes(window, cx);
                                        })),
                                )
                            })
                            .when(!available.is_zero(), |row| {
                                row.child(
                                    app_button_base("swap-amount-max")
                                        .debug_selector(|| "swap-amount-max".into())
                                        .label("Max")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                                        .disabled(!editable)
                                        .tooltip(format!(
                                            "Use {available_label}, the most one swap can spend"
                                        ))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.use_amount(available, window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .children(too_large)
    }

    /// The Buy panel: the raw quote, its token, the price check, and the expected amount after
    /// every cost. The flip button sits on the seam above it.
    fn render_buy_panel(
        &self,
        form: &SwapForm,
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let review = match &form.quote {
            QuoteState::Ready(review) => Some(review),
            _ => None,
        };
        let amount = review.map(|review| {
            self.bare_amount(review.plan().buy_token(), review.quote().buy_amount, cx)
        });
        let balance = form.buy.and_then(|buy| self.private_balance_label(buy, cx));
        amount_panel(false, cx)
            .child(app_muted_text("Buy"))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(match amount {
                        Some(amount) => app_amount_text(amount).truncate(),
                        None => app_amount_text("0").text_color(rgb(theme::TEXT_SUBTLE)),
                    }))
                    .child(
                        token_pill(
                            Combobox::new(&form.buy_select)
                                .w_full()
                                .placeholder("Select asset")
                                .disabled(locked || !editable)
                                .render_trigger(|trigger, _, cx| {
                                    let title = trigger.selection().first().map_or_else(
                                        || {
                                            div()
                                                .child(
                                                    trigger
                                                        .placeholder()
                                                        .cloned()
                                                        .unwrap_or_default(),
                                                )
                                                .into_any_element()
                                        },
                                        |(_, item)| {
                                            item.display_title()
                                                .unwrap_or_else(|| item.title().into_any_element())
                                        },
                                    );
                                    div()
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_color(if trigger.is_disabled() {
                                            cx.theme().muted_foreground
                                        } else {
                                            cx.theme().foreground
                                        })
                                        .child(div().flex_1().min_w_0().child(title))
                                        .child(
                                            Caret::new(trigger.size())
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                })
                                .when(form.buy.is_none() && editable && !locked, |select| {
                                    select
                                        .border_color(cx.theme().primary.opacity(0.65))
                                        .bg(cx.theme().primary.opacity(0.12))
                                }),
                        )
                        .debug_selector(|| "swap-buy-selector".into()),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(
                        matches!(
                            form.quote,
                            QuoteState::Failed(_) | QuoteState::PriceBlocked(_)
                        ),
                        gpui::Styled::items_start,
                    )
                    .child(self.render_price_line(form, cx).flex_1().min_w_0())
                    .children(balance.map(|balance| balance_text(balance).flex_none())),
            )
            .children(review.and_then(|review| {
                if !matches!(
                    self.form_mode(form),
                    FormMode::Setup { .. } | FormMode::Order
                ) {
                    return None;
                }
                Self::render_cost_acknowledgement(form, high_cost_bps(review), editable, cx)
                    .map(gpui::Styled::mt_2)
            }))
            .children(review.map(|review| self.render_receive_strip(form, review, cx)))
    }

    fn render_flip(
        &self,
        form: &SwapForm,
        sell_assets: &[UnshieldAsset],
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let reason = match form.buy {
            _ if locked => Some("This swap's tokens are fixed".to_owned()),
            None => Some("Choose a token to receive first".to_owned()),
            Some(buy)
                if !sell_assets
                    .iter()
                    .any(|asset| asset.token == buy && !asset.max_batched.is_zero()) =>
            {
                Some(format!(
                    "No spendable private {} to sell",
                    self.token_symbol(buy, cx)
                ))
            }
            Some(buy)
                if !self
                    .buy_items(buy, cx)
                    .iter()
                    .any(|item| item.token == form.sell) =>
            {
                Some(format!(
                    "{} can't be received in a private swap",
                    self.token_symbol(form.sell, cx)
                ))
            }
            Some(_) => None,
        };
        let disabled = !editable || reason.is_some();
        let tooltip = reason.unwrap_or_else(|| "Switch the Sell and Buy tokens".to_owned());
        // Paint after the Buy panel as its sibling: GPUI paints a parent's border after its
        // children. Center the small button over the half-gap above the panel.
        div()
            .absolute()
            .left_0()
            .right_0()
            .top(-rems(0.75 + 0.125))
            .flex()
            .justify_center()
            .child(
                app_button_base("swap-flip")
                    .small()
                    .border_1()
                    .border_color(cx.theme().border)
                    .icon(IconName::ArrowDown)
                    .accessibility_label("Switch tokens")
                    .tooltip(tooltip)
                    .disabled(disabled)
                    .debug_selector(|| "swap-flip".into())
                    .on_click(cx.listener(|this, _, window, cx| this.flip_tokens(window, cx))),
            )
    }

    /// The price check under the Buy amount. A failed or missing check replaces the anchor
    /// delta on the same line.
    fn render_price_line(&self, form: &SwapForm, cx: &Context<'_, Self>) -> gpui::Div {
        let retry = || {
            app_button("swap-price-retry", "Retry")
                .debug_selector(|| "swap-price-retry".into())
                .outline()
                .small()
                .flex_none()
                .disabled(self.busy() || form.quote_task.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.retry_quote(window, cx)))
        };
        let line = div()
            .debug_selector(|| "swap-price-status".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x_2()
            .when(
                matches!(
                    form.quote,
                    QuoteState::Failed(_) | QuoteState::PriceBlocked(_)
                ),
                |line| line.flex_col().items_start().gap_y_1(),
            );
        match &form.quote {
            QuoteState::Idle | QuoteState::TooLarge { .. } => line.child(
                app_muted_text(if form.buy.is_none() {
                    "Choose a token to receive"
                } else {
                    "Enter an amount that fits to get a quote"
                })
                .whitespace_normal(),
            ),
            QuoteState::Loading => line
                .child(Spinner::new().small())
                .child(app_muted_text("Getting a quote and checking the price…")),
            QuoteState::Failed(error) => line
                .child(self.render_quote_error(error, cx))
                .child(retry()),
            QuoteState::PriceBlocked(block) => {
                let text = match block {
                    PriceBlock::Deviates => {
                        let threshold = self
                            .swap_profile(cx)
                            .map_or(300, |profile| profile.anchor_deviation_bps());
                        format!(
                            "The exchange rate before fees is more than {} below the anchor price",
                            format_bps_percent(u64::from(threshold))
                        )
                    }
                };
                line.child(
                    app_text(text)
                        .w_full()
                        .min_w_0()
                        .text_color(rgb(theme::WARNING))
                        .whitespace_normal(),
                )
                .child(retry())
            }
            QuoteState::Ready(review) => {
                let buy = review.plan().buy_token();
                match review.price() {
                    SwapPrice::Unverified => line.child(
                        app_text(UNVERIFIED_PRICE_WARNING)
                            .text_color(cx.theme().warning)
                            .whitespace_normal(),
                    ),
                    SwapPrice::Verified { .. } => line
                        .children(
                            self.usd_label(buy, review.quote().buy_amount, cx)
                                .map(app_muted_text),
                        )
                        .children(price_delta(review, cx).map(|(delta, checked)| {
                            div().id("swap-price-delta").child(delta).when_some(
                                checked,
                                |delta, checked| {
                                    delta.tooltip(move |window, cx| {
                                        Tooltip::new(checked.clone()).build(window, cx)
                                    })
                                },
                            )
                        })),
                }
            }
        }
    }

    fn render_quote_error(&self, error: &eyre::Report, cx: &App) -> gpui::Div {
        let message = match error.downcast_ref::<OrderLimitError>() {
            Some(OrderLimitError::HookCostExceedsOutput {
                buy_token,
                hook_cost,
                ..
            }) => format!(
                "Sell amount is too small. Estimated cost: {}",
                self.token_amount(*buy_token, *hook_cost, cx)
            ),
            _ => format!("{error:#}"),
        };
        app_text(message)
            .debug_selector(|| "swap-price-error".into())
            .w_full()
            .min_w_0()
            .text_color(cx.theme().danger)
            .whitespace_normal()
    }

    /// The expected private amount at the quoted price, with the signed minimum beneath it, on a
    /// strip across the bottom of the Buy panel.
    fn render_receive_strip(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let token = review.plan().buy_token();
        let open = form.receive_help_open;
        let view = cx.entity();
        div()
            // Out to the panel's border, past its `px_3` and `py_2p5` padding.
            .mx(rems(-0.75))
            .mb(rems(-0.625))
            .mt_1()
            .px_3()
            .py_2p5()
            .rounded_b_lg()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                Popover::new("swap-receive-help-popover")
                    .open(open)
                    .on_open_change(move |open, _, cx| {
                        view.update(cx, |view, cx| view.set_receive_help_open(*open, cx));
                    })
                    .trigger(
                        app_button_base("swap-receive-help-trigger")
                            .text()
                            .xsmall()
                            .compact()
                            .accessibility_label("About the receive amount")
                            .child(
                                div()
                                    .id("swap-receive-help")
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(app_muted_text("You receive"))
                                    .child(
                                        Icon::new(IconName::Info)
                                            .xsmall()
                                            .text_color(rgb(theme::TEXT_MUTED)),
                                    )
                                    .when(!open, |this| {
                                        this.tooltip(|window, cx| {
                                            Tooltip::element(|window, _| receive_help_card(window))
                                                .build(window, cx)
                                        })
                                    }),
                            ),
                    )
                    .content(|_, window, _| receive_help_card(window)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .child(
                        app_strong_text(format!(
                            "≈ {}",
                            self.bare_amount(token, expected_output(review), cx)
                        ))
                        .text_size(theme::BALANCE_TEXT_SIZE)
                        .font_weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(app_muted_text(format!(
                        "at least {}",
                        self.bare_amount(token, review.suggested_private_minimum(), cx)
                    ))),
            )
    }

    fn render_price_acknowledgement(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<Checkbox> {
        let QuoteState::Ready(review) = &form.quote else {
            return None;
        };
        (*review.price() == SwapPrice::Unverified).then(|| {
            Checkbox::new("swap-price-acknowledged")
                .label("I accept this price without an independent check")
                .checked(form.price_acknowledged)
                .small()
                .disabled(self.busy())
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    if let Some(form) = this.form.as_mut() {
                        form.price_acknowledged = *checked;
                        form.error = None;
                    }
                    cx.notify();
                }))
        })
    }

    fn render_cost_acknowledgement(
        form: &SwapForm,
        cost_bps: Option<u64>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let bps = cost_bps?;
        let warning = cx.theme().warning;
        // Alert accepts text only. One frame contains its message and the standard checkbox.
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(warning.mix_oklab(gpui::transparent_white(), 0.3))
                .bg(warning.mix_oklab(gpui::transparent_white(), 0.04))
                .debug_selector(|| "swap-high-costs".into())
                .child(
                    div()
                        .min_w_0()
                        .debug_selector(|| "swap-high-cost-message".into())
                        .child(
                            Alert::warning("swap-high-costs", high_cost_message(bps))
                                .small()
                                .p_0()
                                .border_0()
                                .bg(gpui::transparent_black()),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "swap-costs-acknowledged".into())
                        .child(
                            Checkbox::new("swap-costs-acknowledged")
                                .label("Swap anyway")
                                .checked(form.high_costs_acknowledged)
                                .small()
                                .disabled(!editable)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(form) = this.form.as_mut() {
                                        form.high_costs_acknowledged = *checked;
                                        form.error = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    /// The stealth account select, the setup broadcaster settings for a new account, and one
    /// line on what the account choice costs or reveals.
    fn render_account_row(
        &self,
        form: &SwapForm,
        mode: FormMode,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let control = match (&form.account_select, form.operation) {
            (Some(select), _) => Select::new(select)
                .w_full()
                .disabled(!editable)
                .into_any_element(),
            (None, Some(operation)) => {
                fixed_account(self.account_label(operation).unwrap_or_default()).into_any_element()
            }
            (None, None) => div().into_any_element(),
        };
        // The line under the select is secondary to it, so it's smaller.
        let line = match mode {
            FormMode::SettingUp => self.render_setting_up_section(form),
            FormMode::Placed => app_muted_text(
                "This swap has an order. Follow it on the Private tab until it ends.",
            )
            .text_xs()
            .whitespace_normal(),
            FormMode::Setup { .. } => app_muted_text(self.setup_line(form, cx))
                .text_xs()
                .whitespace_normal(),
            FormMode::Order if form.reuse_account => self.render_reuse_warning(form),
            FormMode::Order => {
                app_muted_text("Already set up for this swap. No setup fee.").text_xs()
            }
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_muted_text("Stealth account")
                            .w(rems(ACCOUNT_LABEL_WIDTH))
                            .flex_none(),
                    )
                    .child(div().flex_1().min_w_0().child(control))
                    .when(matches!(mode, FormMode::Setup { .. }), |row| {
                        row.child(self.render_setup_settings(form, cx))
                    }),
            )
            .child(if mode == FormMode::SettingUp {
                line
            } else {
                // Under the select, past the label and the row's gap.
                div().pl(rems(ACCOUNT_LABEL_WIDTH + 0.5)).child(line)
            })
    }

    /// The reuse warning; its tooltip holds the full privacy warning the review repeats.
    fn render_reuse_warning(&self, form: &SwapForm) -> gpui::Div {
        let account = form
            .operation
            .and_then(|operation| self.record(operation))
            .map_or_else(
                || "this account".to_owned(),
                |record| format!("#{}", record.index()),
            );
        div().child(
            div()
                .id("swap-account-reuse")
                .child(warning_line(
                    format!("Links this swap to {account}'s earlier activity. No setup fee."),
                    true,
                ))
                .tooltip(|window, cx| Tooltip::new(ACCOUNT_REUSE_NOTE).build(window, cx)),
        )
    }

    /// The gear that opens the setup broadcaster settings: fee token, Random or Specific
    /// broadcaster, favorites only, and out-of-range fees.
    fn render_setup_settings(&self, form: &SwapForm, cx: &Context<'_, Self>) -> Popover {
        let view = cx.entity();
        let open_view = view.clone();
        let choice = form.route.choice();
        let route = SetupRouteSettings {
            fee_options: form.route.fee_options.clone(),
            fee_token: form.route.fee_token.unwrap_or_default(),
            allow_out_of_range: form.route.allow_out_of_range,
            favorites_only: form.route.favorites_only,
            random_selected: form.route.selected.is_none(),
            specific_label: selected_broadcaster_label(&choice, &form.route.candidates),
            candidate_count: form.route.candidates.len(),
            busy: self.busy(),
        };
        Popover::new("swap-setup-settings")
            .anchor(Anchor::TopRight)
            // Presses elsewhere in the form close it; see `render_form`.
            .overlay_closable(false)
            .open(form.settings_open)
            .on_open_change(move |open, _, cx| {
                open_view.update(cx, |view, cx| view.set_setup_settings_open(*open, cx));
            })
            .trigger(
                app_button_base("swap-setup-settings-trigger")
                    .ghost()
                    .icon(IconName::Settings)
                    .accessibility_label("Setup broadcaster")
                    .tooltip("Setup broadcaster"),
            )
            .content(move |_, _, _| setup_route_settings(view.clone(), &route).w(rems(24.)))
    }

    /// The rate and total costs; expanded, the costs taken from the output, the minimum the
    /// review approves, slippage, and the order's validity.
    fn render_details(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        with_setup: bool,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let costs = total_cost(review);
        let open = form.details_open;
        let toggle = if open { "Hide details" } else { "Show details" };
        let details = Collapsible::new()
            .open(open)
            .w_full()
            .min_w_0()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                app_button_base("swap-details-toggle")
                    .ghost()
                    .w_full()
                    .min_w_0()
                    .h_auto()
                    .min_h_8()
                    .px_0()
                    .py_1()
                    .accessibility_label(toggle)
                    .tooltip(toggle)
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                app_text(self.rate_label(review, cx))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate(),
                            )
                            .child(
                                app_muted_text(self.with_usd(
                                    format!("≈ {} in costs", self.token_amount(buy, costs, cx)),
                                    buy,
                                    costs,
                                    cx,
                                ))
                                .flex_none(),
                            )
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall()
                                .flex_none(),
                            ),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_details(cx))),
            );
        if !open {
            return details;
        }
        let hook_cost = review.hook_cost();
        let cow_fee = cow_fee(review);
        let shield_fee = shield_fee_on(review, review.quote().buy_amount);
        let unshield_fee = plan.amount().saturating_sub(review.sell_amount());
        let minutes = self
            .swap_profile(cx)
            .map_or(10, |profile| profile.valid_to_window().as_secs() / 60);
        let minimum = review.suggested_private_minimum();
        // Two tokens, so one USD total, or none unless both have a rate.
        let railgun_fees_usd = self
            .usd_micro_value(sell, unshield_fee, cx)
            .zip(self.usd_micro_value(buy, shield_fee, cx))
            .map(|(unshield, shield)| unshield.saturating_add(shield));
        let content = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(detail_row(
                "CoW network fee",
                app_text(self.with_usd(
                    format!("≈ {}", self.token_amount(buy, cow_fee, cx)),
                    buy,
                    cow_fee,
                    cx,
                )),
                true,
                Some("CoW's quoted fee, already included in the quote.".into()),
            ))
            .child(detail_row(
                "Hook gas",
                app_text(self.with_usd(
                    format!("up to {}", self.token_amount(buy, hook_cost, cx)),
                    buy,
                    hook_cost,
                    cx,
                )),
                true,
                Some(
                    "The hooks' worst-case gas at the quoted gas price. The minimum covers it."
                        .into(),
                ),
            ))
            .child(detail_row(
                "Railgun fees",
                app_text(format_value_with_usd_label(
                    format!(
                        "{} + {}",
                        self.token_amount(sell, unshield_fee, cx),
                        self.token_amount(buy, shield_fee, cx)
                    ),
                    U256::ZERO,
                    // Without decimals the helper skips its stablecoin check, which doesn't
                    // apply to a total across two tokens.
                    None,
                    railgun_fees_usd,
                    false,
                )),
                true,
                Some(railgun_fees_label(review)),
            ))
            .child(detail_row(
                "Receive at least",
                app_text(self.with_usd(self.token_amount(buy, minimum, cx), buy, minimum, cx)),
                false,
                Some("The minimum you approve in the review, after every fee.".into()),
            ))
            .child(detail_row(
                "Slippage",
                Self::render_slippage(form, editable, cx),
                false,
                None,
            ))
            .child(detail_row(
                "Order valid for",
                app_text(if with_setup {
                    format!("{minutes} minutes after setup")
                } else {
                    format!("{minutes} minutes")
                }),
                false,
                None,
            ));
        details.content(content)
    }

    /// Today's slippage presets in a popover. Choosing another preset quotes the swap again. The
    /// quote details, this popover among them, hide until a new quote is ready.
    fn render_slippage(form: &SwapForm, editable: bool, cx: &Context<'_, Self>) -> Popover {
        let view = cx.entity();
        let selected = form.slippage_bps;
        Popover::new("swap-slippage")
            .anchor(Anchor::TopRight)
            .trigger(
                app_button_base("swap-slippage-trigger")
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
                    .accessibility_label("Slippage")
                    .child(app_button_label(format_bps_percent(u64::from(selected)))),
            )
            .content(move |_, _, _| {
                div()
                    .w(rems(16.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_strong_text("Slippage"))
                    .child(
                        ButtonGroup::new("swap-slippage-presets")
                            .outline()
                            .compact()
                            .children(SLIPPAGE_CHOICES.into_iter().map(|(bps, label)| {
                                let view = view.clone();
                                app_segment_button(
                                    SharedString::from(format!("swap-slippage-{bps}")),
                                    label,
                                    selected == bps,
                                    !editable,
                                    None,
                                )
                                .on_click(move |_, window, cx| {
                                    view.update(cx, |view, cx| view.set_slippage(bps, window, cx));
                                })
                            })),
                    )
                    .child(
                        app_muted_text(
                            "Lower slippage fills less often. An unfilled order costs only the setup.",
                        )
                        .whitespace_normal(),
                    )
            })
    }

    /// Show what keeps notes out of this swap, above the form.
    fn open_locked_notes(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        crate::root::locked_notes::open_locked_notes_dialog(
            self.root.clone(),
            Arc::clone(&self.session),
            self.runtime.clone(),
            window,
            cx,
        );
    }

    /// The amount-reduction prompt next to the amount it describes.
    fn render_too_large(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let QuoteState::TooLarge { largest } = form.quote else {
            return None;
        };
        let entered = self.form_amount(form, cx).map_or_else(
            |_| "This amount".into(),
            |amount| self.token_amount(form.sell, amount, cx),
        );
        let largest_label = self.token_amount(form.sell, largest, cx);
        Some(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    app_text(format!(
                        "{entered} is spread across more notes than one swap can spend. Up to {largest_label} fits."
                    ))
                    .flex_1()
                    .min_w_0()
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal(),
                )
                .when(!largest.is_zero(), |row| {
                    row.child(
                        app_button("swap-use-largest", format!("Use {largest_label}"))
                            .small()
                            .flex_none()
                            .disabled(!editable)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.use_amount(largest, window, cx);
                            })),
                    )
                }),
        )
    }

    fn render_setting_up_section(&self, form: &SwapForm) -> gpui::Div {
        let tracking = form
            .operation
            .and_then(|operation| self.tracking.get(&operation));
        let stage = tracking
            .and_then(|tracking| tracking.setup_stage.as_ref())
            .map(|receiver| *receiver.borrow());
        let status = stage.map_or(
            "Waiting for the setup to be confirmed",
            TransactionGenerationStage::label,
        );
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(app_text(format!("Setting up the swap's stealth account · {status}"))),
            )
            .children(
                tracking
                    .and_then(|tracking| tracking.error.clone())
                    .map(|error| {
                        app_muted_text(error)
                            .text_color(rgb(theme::DANGER))
                            .whitespace_normal()
                    }),
            )
            .child(
                app_muted_text(
                    "Confirmation takes a few minutes. You can close this; the swap stays on the Private tab, and its order is placed once the setup is confirmed.",
                )
                .whitespace_normal(),
            )
    }
}

fn select_index(
    items: &[PrivateActionAssetSelectItem],
    token: Address,
) -> Option<gpui_component::IndexPath> {
    items
        .iter()
        .position(|item| item.token == token)
        .map(|index| gpui_component::IndexPath::default().row(index))
}

/// What the setup broadcaster popover shows, captured when the form renders.
struct SetupRouteSettings {
    fee_options: Vec<PublicBroadcasterFeeTokenOption>,
    fee_token: Address,
    allow_out_of_range: bool,
    favorites_only: bool,
    random_selected: bool,
    specific_label: String,
    candidate_count: usize,
    busy: bool,
}

/// The shared broadcaster settings with the fee token selector, driving the swap's setup route.
/// Specific broadcaster opens the broadcaster picker on top of the swap dialog.
fn setup_route_settings(
    view: Entity<PrivateSwapsView>,
    route: &SetupRouteSettings,
) -> gpui::Stateful<gpui::Div> {
    use ui::private_action::BroadcasterSettingsEvent as Event;
    let fee_view = view.clone();
    let fee_token = fee_token_selector(
        "swap-setup-fee-token".into(),
        &route.fee_options,
        route.fee_token,
        route.busy,
        move |token, _, cx| {
            fee_view.update(cx, |view, cx| {
                if let Some(form) = view.form.as_mut() {
                    form.route.fee_token = Some(token);
                    form.route.invalidate_estimate();
                }
                view.refresh_setup_route(cx);
            });
        },
    );
    ui::private_action::broadcaster_settings_fields(
        "swap-setup-broadcaster-settings",
        ui::private_action::BroadcasterSettings {
            allow_out_of_range: route.allow_out_of_range,
            favorites_only: route.favorites_only,
            random_selected: route.random_selected,
            specific_label: route.specific_label.clone(),
            candidate_count: route.candidate_count,
            disabled: route.busy,
        },
        fee_token,
        None,
        move |event, window, cx| {
            view.update(cx, |view, cx| {
                if matches!(event, Event::ChooseSpecific) {
                    view.choose_specific_setup_broadcaster(window, cx);
                    return;
                }
                let Some(form) = view.form.as_mut() else {
                    return;
                };
                match event {
                    Event::Random => form.route.selected = None,
                    Event::AllowOutOfRange(value) => form.route.allow_out_of_range = value,
                    Event::FavoritesOnly(value) => form.route.favorites_only = value,
                    Event::ChooseSpecific => {}
                }
                form.route.invalidate_estimate();
                view.refresh_setup_route(cx);
            });
        },
    )
}

/// The quote against the anchor price, such as "(−0.4% vs Chainlink)", and the reading behind
/// it. Chainlink is named only when every anchor reading came from a Chainlink round.
fn price_delta(review: &SwapReview, cx: &App) -> Option<(gpui::Div, Option<String>)> {
    let SwapPrice::Verified { rate, observations } = review.price() else {
        return None;
    };
    let expected = if rate.sell_rate.is_zero() {
        U256::ZERO
    } else {
        review.quote().sell_amount.saturating_mul(rate.buy_rate) / rate.sell_rate
    };
    let anchor = if !observations.is_empty()
        && observations
            .iter()
            .all(|observation| observation.updated_at.is_some())
    {
        "Chainlink"
    } else {
        "the anchor price"
    };
    let delta = quote_anchor_delta_bps(review.quote().buy_amount, expected).map_or_else(
        || app_muted_text(format!("(within range of {anchor})")),
        |delta| {
            let color = match delta.cmp(&0) {
                std::cmp::Ordering::Less => cx.theme().danger,
                std::cmp::Ordering::Equal => cx.theme().muted_foreground,
                std::cmp::Ordering::Greater => cx.theme().success,
            };
            div()
                .flex()
                .items_baseline()
                .child(app_muted_text("("))
                .child(
                    app_text(format!(
                        "{}{}",
                        if delta < 0 { "−" } else { "+" },
                        format_bps_percent(delta.unsigned_abs())
                    ))
                    .text_color(color),
                )
                .child(app_muted_text(format!(" vs {anchor})")))
        },
    );
    let checked = observations
        .iter()
        .map(|observation| observation.block.number)
        .max()
        .map(|block| format!("Checked against {anchor} at block {block}"));
    Some((delta, checked))
}

/// The swap fees known up front, at the quoted trading rate: `CoW`'s quoted fee and Railgun's
/// unshield and shield fees. Setup is paid separately.
fn total_cost(review: &SwapReview) -> U256 {
    swap_fees_to(review, expected_output(review))
}

/// The known fees plus the hook gas cap, at the quoted trading rate.
fn worst_case_cost(review: &SwapReview) -> U256 {
    swap_fees_to(review, worst_case_output(review))
}

/// The sell-token fees and every deduction from the quote down to `received`.
fn swap_fees_to(review: &SwapReview, received: U256) -> U256 {
    let unshield_fee = review.plan().amount().saturating_sub(review.sell_amount());
    swap_total_cost(
        unshield_fee.saturating_add(review.quote().fee_amount),
        review.quote().sell_amount,
        review.quote().buy_amount,
        received,
    )
}

/// `CoW`'s quoted fee in buy-token base units. The quote already includes it.
fn cow_fee(review: &SwapReview) -> U256 {
    swap_total_cost(
        review.quote().fee_amount,
        review.quote().sell_amount,
        review.quote().buy_amount,
        review.quote().buy_amount,
    )
}

/// Warns on the worst case, with the hook gas cap counted as a cost.
fn high_cost_bps(review: &SwapReview) -> Option<u64> {
    let bps = swap_cost_bps(worst_case_cost(review), worst_case_output(review));
    (bps >= 1_000).then_some(bps)
}

fn high_cost_message(bps: u64) -> String {
    format!(
        "Swap costs could reach {} of the swap amount, counting the full hook gas estimate.",
        format_bps_percent(bps)
    )
}

/// The expected private amount at the quoted price: the quote, which already includes `CoW`'s
/// fee, less Railgun's shield fee. The minimum covers hook gas instead.
fn expected_output(review: &SwapReview) -> U256 {
    let quoted = review.quote().buy_amount;
    quoted.saturating_sub(shield_fee_on(review, quoted))
}

/// The private amount at the quoted price if hook gas reaches its estimate: the quote less the
/// hook cost and Railgun's shield fee.
fn worst_case_output(review: &SwapReview) -> U256 {
    let after_hooks = review.quote().buy_amount.saturating_sub(review.hook_cost());
    after_hooks.saturating_sub(shield_fee_on(review, after_hooks))
}

/// Railgun's shield fee on a buy-token amount.
fn shield_fee_on(review: &SwapReview, amount: U256) -> U256 {
    amount.saturating_mul(review.shield_fee_bps()) / U256::from(10_000u32)
}

/// A rounded Sell or Buy panel, raised above the dialog. A danger border marks an
/// amount that doesn't fit one swap.
fn amount_panel(danger: bool, cx: &App) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2p5()
        .rounded_lg()
        .border_1()
        .border_color(if danger {
            cx.theme().danger
        } else {
            cx.theme().border
        })
        .bg(cx.theme().group_box)
}

/// A panel's token select, at its trailing edge.
fn token_pill(select: impl IntoElement) -> gpui::Div {
    div().w(rems(9.)).flex_none().child(select)
}

/// A started swap's own stealth account, which the form can't change.
fn fixed_account(label: String) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .h_8()
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            app_muted_text(label)
                .min_w_0()
                .truncate()
                .font_family(theme::APP_MONO_FONT_FAMILY),
        )
}

/// A warning in the warning color. A `small` one is a secondary line, such as the one under the
/// stealth account select.
fn warning_line(text: impl Into<SharedString>, small: bool) -> gpui::Div {
    let icon = Icon::new(IconName::TriangleAlert).text_color(rgb(theme::WARNING));
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(if small { icon.xsmall() } else { icon.small() })
        .child(
            app_text(text)
                .when(small, gpui::Styled::text_xs)
                .text_color(rgb(theme::WARNING))
                .whitespace_normal(),
        )
}

/// A panel's balance: secondary to the amount beside it, so muted and smaller.
fn balance_text(text: impl Into<SharedString>) -> gpui::Div {
    app_muted_text(text).text_xs()
}

/// What the receive strip's estimate already accounts for, for its hover tooltip and pinned
/// popover.
fn receive_help_card(window: &Window) -> gpui::Div {
    ui::hint::hint_card("You receive", theme::INFO, window).child(div().child(
        "The estimate is CoW's quote with its fee and Railgun's shield fee already taken out.",
    ))
}

/// One details row. Indented rows are the costs taken from the output; `help` explains the
/// value in a tooltip.
fn detail_row(
    label: &'static str,
    value: impl IntoElement,
    indented: bool,
    help: Option<String>,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .when(indented, gpui::Styled::pl_4)
        .child(
            div()
                .id(label)
                .flex()
                .items_center()
                .gap_1()
                .child(app_muted_text(label))
                .when_some(help, |label, help| {
                    label
                        .child(
                            Icon::new(IconName::Info)
                                .xsmall()
                                .text_color(rgb(theme::TEXT_MUTED)),
                        )
                        .tooltip(move |window, cx| Tooltip::new(help.clone()).build(window, cx))
                }),
        )
        .child(div().flex_none().child(value))
}

fn railgun_fees_label(review: &SwapReview) -> String {
    format!(
        "{} unshield, {} shield",
        format_bps_percent(u64::try_from(review.unshield_fee_bps()).unwrap_or(u64::MAX)),
        format_bps_percent(u64::try_from(review.shield_fee_bps()).unwrap_or(u64::MAX))
    )
}

const fn review_change_label(change: SwapReviewChange) -> &'static str {
    match change {
        SwapReviewChange::HookCost => "the network and hook limit needs review",
        SwapReviewChange::ShieldFee { .. } => "the Railgun shield fee changed",
        SwapReviewChange::UnshieldFee { .. } => "the Railgun unshield fee changed",
        SwapReviewChange::QuoteDeviates => "the quote now deviates from the anchor price",
        SwapReviewChange::PriceVerification => "the price check changed",
        SwapReviewChange::PriceUnavailable => "the independent price is unavailable",
        SwapReviewChange::Minimum { .. } => {
            "the current quote no longer supports the minimum you approved"
        }
    }
}

/// What a broadcaster's answer leaves for the user to know. `None` when it accepted.
pub(super) fn broadcaster_result_problem(
    result: &PublicBroadcasterResultKind,
    what: &str,
) -> Option<String> {
    match result {
        PublicBroadcasterResultKind::Submitted { .. } => None,
        PublicBroadcasterResultKind::Failed { error } => Some(format!(
            "The broadcaster reported a problem with the {what}: {error}"
        )),
        PublicBroadcasterResultKind::TimedOut => Some(format!(
            "No broadcaster response yet. The {what} stays tracked, and the swap updates when it lands."
        )),
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;
