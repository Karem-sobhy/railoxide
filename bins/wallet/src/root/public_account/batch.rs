use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Weak;
use std::sync::Arc;

use alloy::primitives::Address;
use gpui::{
    Context, Entity, IntoElement, ParentElement, Pixels, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    Disableable, Selectable, Sizable, button::ButtonVariants, checkbox::Checkbox,
    input::InputGroupButton, scroll::ScrollableElement,
};
use railgun_ui::short_address;
use ui::controls::{app_button, app_input, app_muted_text, app_strong_text};
use ui::theme::APP_MONO_FONT_FAMILY;
use wallet_ops::vault::{
    DERIVED_ADDRESS_BROWSE_PAGE_SIZE, MAX_DERIVED_ADDRESS_BATCH_COUNT,
    derived_address_page_indexes, validate_derived_address_range,
};
use zeroize::Zeroizing;

use super::types::PublicAccountFormState;
use crate::root::WalletRoot;
use crate::root::device_auth::{
    DEVICE_AUTH_REASON_PUBLIC_ACCOUNT, DeviceAuthMethod, DeviceAuthPassword, device_auth_buttons,
};

/// Which derivation UI the Add Account dialog shows. Single-address
/// derivation is unchanged; Browse and Range reuse the same current private
/// wallet key material and the same vault password input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::root) enum DeriveAccountMode {
    #[default]
    Single,
    Browse,
    Range,
}

impl DeriveAccountMode {
    pub(in crate::root) const fn label(self) -> &'static str {
        match self {
            Self::Single => "Single",
            Self::Browse => "Browse",
            Self::Range => "Range",
        }
    }
}

/// One row in the browse list: the derived address plus whether the wallet
/// already contains it (and is therefore not selectable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::root) struct DerivedBrowseRow {
    pub(in crate::root) derivation_index: u32,
    pub(in crate::root) address: Address,
    pub(in crate::root) already_added: bool,
}

/// Batch derivation UI state owned by [`PublicAccountFormState`].
pub(in crate::root) struct DeriveBatchState {
    pub(in crate::root) mode: DeriveAccountMode,
    /// 1-indexed browse page. Page 1 covers indexes 0–19.
    pub(in crate::root) page: u32,
    pub(in crate::root) page_input: Entity<gpui_component::input::InputState>,
    pub(in crate::root) rows: Vec<DerivedBrowseRow>,
    /// Selected derivation indexes across pages (already-added excluded).
    pub(in crate::root) selected: BTreeSet<u32>,
    pub(in crate::root) loading: bool,
    pub(in crate::root) range_start_input: Entity<gpui_component::input::InputState>,
    pub(in crate::root) range_count_input: Entity<gpui_component::input::InputState>,
    /// Last batch result or validation message for the active mode.
    pub(in crate::root) feedback: Option<Arc<str>>,
}

impl DeriveBatchState {
    pub(in crate::root) fn selectable_rows(&self) -> impl Iterator<Item = &DerivedBrowseRow> {
        self.rows.iter().filter(|row| !row.already_added)
    }

    pub(in crate::root) fn selectable_page_indexes(&self) -> Vec<u32> {
        self.selectable_rows()
            .map(|row| row.derivation_index)
            .collect()
    }

    pub(in crate::root) fn selected_on_page(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| self.selected.contains(&row.derivation_index))
            .count()
    }

    pub(in crate::root) fn page_fully_selected(&self) -> bool {
        let selectable = self.selectable_page_indexes();
        !selectable.is_empty() && selectable.iter().all(|index| self.selected.contains(index))
    }
}

pub(in crate::root) const fn default_derive_batch_state(
    page_input: Entity<gpui_component::input::InputState>,
    range_start_input: Entity<gpui_component::input::InputState>,
    range_count_input: Entity<gpui_component::input::InputState>,
) -> DeriveBatchState {
    DeriveBatchState {
        mode: DeriveAccountMode::Single,
        page: 1,
        page_input,
        rows: Vec::new(),
        selected: BTreeSet::new(),
        loading: false,
        range_start_input,
        range_count_input,
        feedback: None,
    }
}

/// Parse a page-number input. Pages start at 1.
pub(in crate::root) fn parse_browse_page(text: &str) -> Option<u32> {
    let page: u32 = text.trim().parse().ok()?;
    (page >= 1).then_some(page)
}

/// Parse the range form. Returns descriptive errors for the UI.
pub(in crate::root) fn parse_range_inputs(
    start_text: &str,
    count_text: &str,
) -> Result<(u32, u32), &'static str> {
    let start: u32 = start_text
        .trim()
        .parse()
        .map_err(|_| "Start index must be a non-negative integer")?;
    let count: u32 = count_text
        .trim()
        .parse()
        .map_err(|_| "Count must be a positive integer")?;
    if count == 0 {
        return Err("Count must be a positive integer");
    }
    if count > MAX_DERIVED_ADDRESS_BATCH_COUNT {
        return Err("Count exceeds the per-batch limit (100)");
    }
    validate_derived_address_range(start, count).map_err(|_| "Start index is out of range")?;
    Ok((start, count))
}

/// Batch action that can be authorized with device authentication instead of
/// typing the vault password. Mirrors the single-add device-auth path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::root) enum DeriveBatchDeviceAction {
    PreviewPage,
    AddSelected,
    AddRange,
}

impl PublicAccountFormState {
    pub(in crate::root) fn reset_derive_batch(&mut self) {
        self.batch.mode = DeriveAccountMode::Single;
        self.batch.page = 1;
        self.batch.rows.clear();
        self.batch.selected.clear();
        self.batch.loading = false;
        self.batch.feedback = None;
    }
}

impl WalletRoot {
    fn derive_batch_password(&self, cx: &Context<'_, Self>) -> Zeroizing<String> {
        Zeroizing::new(
            self.public_form
                .add_password_input
                .read(cx)
                .value()
                .to_string(),
        )
    }

    /// Mark previews as already-added using the wallet's existing
    /// address/account state: a derived index present in this wallet's scope
    /// (active or inactive), or an address already active for the wallet.
    fn derive_batch_mark_rows(
        &self,
        previews: Vec<wallet_ops::vault::DerivedAddressPreview>,
    ) -> Vec<DerivedBrowseRow> {
        let Some(view_session) = self.view_session.as_ref() else {
            return previews
                .into_iter()
                .map(|preview| DerivedBrowseRow {
                    derivation_index: preview.derivation_index,
                    address: preview.address,
                    already_added: true,
                })
                .collect();
        };
        let wallet_id = view_session.wallet_id();
        previews
            .into_iter()
            .map(|preview| {
                let already_added = self.public_accounts.iter().any(|account| {
                    let same_derived_index = matches!(
                        account.source,
                        wallet_ops::vault::PublicAccountSource::Derived
                            | wallet_ops::vault::PublicAccountSource::HardwareDerived
                    ) && matches!(
                        &account.scope,
                        wallet_ops::vault::PublicAccountScope::PrivateWallet {
                            wallet_uuid: scoped,
                        } if scoped == wallet_id
                    ) && account.derivation_index
                        == Some(preview.derivation_index);
                    same_derived_index
                        || (account.is_active_for_wallet(wallet_id)
                            && account.address == preview.address)
                });
                DerivedBrowseRow {
                    derivation_index: preview.derivation_index,
                    address: preview.address,
                    already_added,
                }
            })
            .collect()
    }

    fn derive_batch_store_preview(
        &self,
        password: &str,
        indexes: &[u32],
    ) -> Result<Vec<DerivedBrowseRow>, String> {
        let store = self
            .vault_store
            .clone()
            .ok_or_else(|| "Wallet vault storage is unavailable".to_owned())?;
        let view_session = self
            .view_session
            .clone()
            .ok_or_else(|| "Wallet vault is locked".to_owned())?;
        if self.selected_hardware_public_device_kind().is_some() {
            return Err("Batch derivation is unavailable for hardware wallets".to_owned());
        }
        let protected = self.protected_software_seed_session.clone();
        store
            .preview_derived_public_addresses(
                password,
                view_session.as_ref(),
                indexes,
                protected.as_deref(),
            )
            .map_err(|error| error.to_string())
            .map(|previews| self.derive_batch_mark_rows(previews))
    }

    pub(in crate::root) fn set_derive_account_mode(
        &mut self,
        mode: DeriveAccountMode,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.batch.mode = mode;
        self.public_form.batch.feedback = None;
        self.public_form.error = None;
        if mode == DeriveAccountMode::Browse && self.public_form.batch.rows.is_empty() {
            // Attempt an immediate preview so returning users don't need an
            // extra click; a missing password just leaves the hint visible.
            let password = self.derive_batch_password(cx);
            if !password.trim().is_empty() {
                self.preview_derived_browse_page(Some(password), window, cx);
                return;
            }
        }
        cx.notify();
    }

    /// Derive and display the addresses for the current browse page.
    pub(in crate::root) fn preview_derived_browse_page(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        _window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.batch.loading {
            return;
        }
        let password = match device_auth_password {
            Some(password) => password,
            None => self.derive_batch_password(cx),
        };
        if password.trim().is_empty() {
            self.public_form.batch.feedback = Some(Arc::from(
                "Enter the vault password to show derived addresses",
            ));
            cx.notify();
            return;
        }
        let page = self.public_form.batch.page;
        let Some(indexes) = derived_address_page_indexes(page) else {
            self.public_form.batch.feedback =
                Some(Arc::from("Enter a valid page number (starting at 1)"));
            cx.notify();
            return;
        };
        self.public_form.batch.loading = true;
        self.public_form.batch.feedback = None;
        cx.notify();
        let rows = self.derive_batch_store_preview(password.as_str(), &indexes);
        self.public_form.batch.loading = false;
        match rows {
            Ok(rows) => {
                self.public_form.batch.rows = rows;
                // Drop selections that are now already added.
                let added: BTreeSet<u32> = self
                    .public_form
                    .batch
                    .rows
                    .iter()
                    .filter(|row| row.already_added)
                    .map(|row| row.derivation_index)
                    .collect();
                self.public_form
                    .batch
                    .selected
                    .retain(|index| !added.contains(index));
            }
            Err(message) => {
                self.public_form.batch.rows.clear();
                self.public_form.batch.feedback = Some(Arc::from(message));
            }
        }
        cx.notify();
    }

    pub(in crate::root) fn goto_derived_browse_page_from_input(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let text = self
            .public_form
            .batch
            .page_input
            .read(cx)
            .value()
            .to_string();
        if let Some(page) = parse_browse_page(&text) {
            if derived_address_page_indexes(page).is_none() {
                self.public_form.batch.feedback =
                    Some(Arc::from("Enter a valid page number (starting at 1)"));
                cx.notify();
                return;
            }
            self.public_form.batch.page = page;
            self.preview_derived_browse_page(None, window, cx);
        } else {
            self.public_form.batch.feedback =
                Some(Arc::from("Enter a valid page number (starting at 1)"));
            cx.notify();
        }
    }

    pub(in crate::root) fn toggle_derived_browse_selection(
        &mut self,
        index: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let selectable = self
            .public_form
            .batch
            .rows
            .iter()
            .any(|row| row.derivation_index == index && !row.already_added);
        if !selectable {
            return;
        }
        if !self.public_form.batch.selected.remove(&index) {
            self.public_form.batch.selected.insert(index);
        }
        cx.notify();
    }

    pub(in crate::root) fn select_all_derived_browse_page(&mut self, cx: &mut Context<'_, Self>) {
        for index in self.public_form.batch.selectable_page_indexes() {
            self.public_form.batch.selected.insert(index);
        }
        cx.notify();
    }

    pub(in crate::root) fn clear_derived_browse_page_selection(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let page_indexes: BTreeSet<u32> = self
            .public_form
            .batch
            .rows
            .iter()
            .map(|row| row.derivation_index)
            .collect();
        self.public_form
            .batch
            .selected
            .retain(|index| !page_indexes.contains(index));
        cx.notify();
    }

    fn finish_derived_batch_add(
        &mut self,
        outcome: &wallet_ops::vault::DerivedBatchAddOutcome,
        submitted: &[u32],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let submitted_set: BTreeSet<u32> = submitted.iter().copied().collect();
        self.public_form
            .batch
            .selected
            .retain(|index| !submitted_set.contains(index));
        if let Some(last) = outcome.added.last() {
            self.public_form.selected_account_uuid =
                Some(Arc::from(last.public_account_uuid.as_str()));
        }
        self.public_form.batch.feedback = Some(Arc::from(outcome.summary()));
        self.public_form.batch.loading = false;
        self.reload_public_accounts(window, cx);
        self.schedule_public_balance_refresh(cx);
        // Refresh already-added flags on the visible page without asking for
        // the password again (it stays in the input until the dialog closes).
        let password = self.derive_batch_password(cx);
        if !password.trim().is_empty() && self.public_form.batch.mode == DeriveAccountMode::Browse {
            let page = self.public_form.batch.page;
            if let Some(indexes) = derived_address_page_indexes(page) {
                match self.derive_batch_store_preview(password.as_str(), &indexes) {
                    Ok(rows) => {
                        self.public_form.batch.rows = rows;
                        let added: BTreeSet<u32> = self
                            .public_form
                            .batch
                            .rows
                            .iter()
                            .filter(|row| row.already_added)
                            .map(|row| row.derivation_index)
                            .collect();
                        self.public_form
                            .batch
                            .selected
                            .retain(|index| !added.contains(index));
                    }
                    Err(message) => {
                        self.public_form.batch.feedback = Some(Arc::from(message));
                    }
                }
            }
        }
        cx.notify();
    }

    /// Add every selected address (across pages) from the current private key.
    pub(in crate::root) fn add_selected_derived_addresses(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.batch.loading {
            return;
        }
        let selected: Vec<u32> = self.public_form.batch.selected.iter().copied().collect();
        if selected.is_empty() {
            self.public_form.batch.feedback = Some(Arc::from("Select at least one address to add"));
            cx.notify();
            return;
        }
        if selected.len() > MAX_DERIVED_ADDRESS_BATCH_COUNT as usize {
            self.public_form.batch.feedback = Some(Arc::from(format!(
                "Select at most {MAX_DERIVED_ADDRESS_BATCH_COUNT} addresses per batch"
            )));
            cx.notify();
            return;
        }
        let password = match device_auth_password {
            Some(password) => password,
            None => self.derive_batch_password(cx),
        };
        if password.trim().is_empty() {
            self.public_form.batch.feedback =
                Some(Arc::from("Enter the vault password to add addresses"));
            cx.notify();
            return;
        }
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.public_form.batch.feedback = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        self.public_form.batch.loading = true;
        self.public_form.batch.feedback = None;
        cx.notify();
        let protected = self.protected_software_seed_session.clone();
        let result = store.add_derived_public_accounts_at_indexes(
            password.as_str(),
            view_session.as_ref(),
            &selected,
            protected.as_deref(),
        );
        match result {
            Ok(outcome) => self.finish_derived_batch_add(&outcome, &selected, window, cx),
            Err(error) => {
                self.public_form.batch.loading = false;
                self.public_form.batch.feedback = Some(Arc::from(error.to_string()));
                cx.notify();
            }
        }
    }

    /// Derive and add a `start..start+count` range from the current private key.
    pub(in crate::root) fn add_derived_address_range(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.batch.loading {
            return;
        }
        let (start_text, count_text) = (
            self.public_form
                .batch
                .range_start_input
                .read(cx)
                .value()
                .to_string(),
            self.public_form
                .batch
                .range_count_input
                .read(cx)
                .value()
                .to_string(),
        );
        let (start, count) = match parse_range_inputs(&start_text, &count_text) {
            Ok(range) => range,
            Err(message) => {
                self.public_form.batch.feedback = Some(Arc::from(message));
                cx.notify();
                return;
            }
        };
        let password = match device_auth_password {
            Some(password) => password,
            None => self.derive_batch_password(cx),
        };
        if password.trim().is_empty() {
            self.public_form.batch.feedback =
                Some(Arc::from("Enter the vault password to add addresses"));
            cx.notify();
            return;
        }
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.public_form.batch.feedback = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        self.public_form.batch.loading = true;
        self.public_form.batch.feedback = None;
        cx.notify();
        let protected = self.protected_software_seed_session.clone();
        let result = store.add_derived_public_accounts_in_range(
            password.as_str(),
            view_session.as_ref(),
            start,
            count,
            protected.as_deref(),
        );
        match result {
            Ok(outcome) => {
                let submitted: Vec<u32> = (0..count).map(|offset| start + offset).collect();
                self.finish_derived_batch_add(&outcome, &submitted, window, cx);
            }
            Err(error) => {
                self.public_form.batch.loading = false;
                self.public_form.batch.feedback = Some(Arc::from(error.to_string()));
                cx.notify();
            }
        }
    }

    pub(in crate::root) fn derive_batch_device_auth_buttons(
        &self,
        root: Entity<Self>,
        action: DeriveBatchDeviceAction,
        id: &'static str,
        busy: bool,
        lease: Weak<Cell<bool>>,
    ) -> Vec<InputGroupButton> {
        device_auth_buttons(
            self.device_auth_prompt_cached().as_ref(),
            id,
            self.device_auth_in_progress,
            busy,
            move |method, window, cx| {
                root.update(cx, |root, cx| {
                    root.submit_derive_batch_with_device_auth(
                        method,
                        action,
                        lease.clone(),
                        window,
                        cx,
                    );
                });
            },
        )
    }

    /// Runs a batch action with the vault password from device authentication.
    /// Validates first so a form that cannot be submitted never prompts.
    fn submit_derive_batch_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        action: DeriveBatchDeviceAction,
        lease: Weak<Cell<bool>>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth_in_progress
            || self.public_form.batch.loading
            || !lease.upgrade().is_some_and(|open| open.get())
        {
            return;
        }
        if self.selected_hardware_public_device_kind().is_some() {
            self.public_form.batch.feedback = Some(Arc::from(
                "Batch derivation is unavailable for hardware wallets",
            ));
            cx.notify();
            return;
        }
        let error: Option<&str> = match action {
            DeriveBatchDeviceAction::PreviewPage => {
                let text = self
                    .public_form
                    .batch
                    .page_input
                    .read(cx)
                    .value()
                    .to_string();
                if parse_browse_page(&text).is_none() {
                    Some("Enter a valid page number (starting at 1)")
                } else {
                    None
                }
            }
            DeriveBatchDeviceAction::AddSelected => {
                let selected = self.public_form.batch.selected.len();
                if selected == 0 {
                    Some("Select at least one address to add")
                } else if selected > MAX_DERIVED_ADDRESS_BATCH_COUNT as usize {
                    Some("Select at most 100 addresses per batch")
                } else {
                    None
                }
            }
            DeriveBatchDeviceAction::AddRange => {
                let start_text = self
                    .public_form
                    .batch
                    .range_start_input
                    .read(cx)
                    .value()
                    .to_string();
                let count_text = self
                    .public_form
                    .batch
                    .range_count_input
                    .read(cx)
                    .value()
                    .to_string();
                parse_range_inputs(&start_text, &count_text).err()
            }
        };
        if let Some(error) = error {
            self.public_form.batch.feedback = Some(Arc::from(error));
            cx.notify();
            return;
        }
        let Some(prompt) = self.device_auth_prompt() else {
            cx.notify();
            return;
        };
        self.device_auth_in_progress = true;
        self.public_form.batch.feedback = None;
        let generation = self.active_wallet_generation;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_PUBLIC_ACCOUNT,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_derive_batch_device_auth(
                    action, &lease, generation, outcome, window, cx,
                );
            },
        );
    }

    fn finish_derive_batch_device_auth(
        &mut self,
        action: DeriveBatchDeviceAction,
        lease: &Weak<Cell<bool>>,
        generation: u64,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_in_progress = false;
        cx.notify();
        if !lease.upgrade().is_some_and(|open| open.get())
            || self.active_wallet_generation != generation
        {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => match action {
                DeriveBatchDeviceAction::PreviewPage => {
                    let text = self
                        .public_form
                        .batch
                        .page_input
                        .read(cx)
                        .value()
                        .to_string();
                    let Some(page) = parse_browse_page(&text) else {
                        self.public_form.batch.feedback =
                            Some(Arc::from("Enter a valid page number (starting at 1)"));
                        cx.notify();
                        return;
                    };
                    self.public_form.batch.page = page;
                    self.preview_derived_browse_page(Some(password), window, cx);
                }
                DeriveBatchDeviceAction::AddSelected => {
                    self.add_selected_derived_addresses(Some(password), window, cx);
                }
                DeriveBatchDeviceAction::AddRange => {
                    self.add_derived_address_range(Some(password), window, cx);
                }
            },
            DeviceAuthPassword::Cancelled => {}
            DeviceAuthPassword::Failed(message) => {
                self.refresh_device_auth_status();
                self.public_form.batch.feedback = Some(message);
            }
        }
    }

    /// Entry point for the Browse/Range buttons. Uses the typed password when
    /// present; otherwise falls back to device authentication when it is set
    /// up, so device-auth users are never forced to type the password.
    pub(in crate::root) fn run_derive_batch_action(
        &mut self,
        action: DeriveBatchDeviceAction,
        lease: Weak<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let typed_empty = self
            .public_form
            .add_password_input
            .read(cx)
            .value()
            .trim()
            .is_empty();
        if typed_empty {
            let method = self.device_auth_prompt_cached().and_then(|prompt| {
                DeviceAuthMethod::ALL
                    .iter()
                    .copied()
                    .find(|method| prompt.includes(*method))
            });
            if let Some(method) = method {
                self.submit_derive_batch_with_device_auth(method, action, lease, window, cx);
                return;
            }
        }
        match action {
            DeriveBatchDeviceAction::PreviewPage => {
                self.goto_derived_browse_page_from_input(window, cx);
            }
            DeriveBatchDeviceAction::AddSelected => {
                self.add_selected_derived_addresses(None, window, cx);
            }
            DeriveBatchDeviceAction::AddRange => {
                self.add_derived_address_range(None, window, cx);
            }
        }
    }

    pub(in crate::root) fn render_derive_mode_selector(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> impl IntoElement {
        let modes = [
            DeriveAccountMode::Single,
            DeriveAccountMode::Browse,
            DeriveAccountMode::Range,
        ];
        let active = self.public_form.batch.mode;
        let mut row = div()
            .w(content_width)
            .flex()
            .items_center()
            .gap_2()
            .child(app_muted_text("Mode:"));
        for mode in modes {
            let mode_root = root.clone();
            let selected = mode == active;
            row = row.child(
                app_button(
                    SharedString::from(format!(
                        "wallet-public-derive-mode-{}",
                        mode.label().to_ascii_lowercase()
                    )),
                    mode.label(),
                )
                .small()
                .compact()
                .selected(selected)
                .when(selected, ButtonVariants::primary)
                .on_click(move |_event, window, cx| {
                    mode_root.update(cx, |root, cx| {
                        root.set_derive_account_mode(mode, window, cx);
                    });
                }),
            );
        }
        row
    }

    pub(in crate::root) fn render_derive_browse_section(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
        lease: Weak<Cell<bool>>,
    ) -> impl IntoElement {
        let state = &self.public_form.batch;
        let loading = state.loading || self.public_form.adding_account;
        let authenticating = self.device_auth_in_progress;
        let page = state.page;
        let page_start = page
            .checked_sub(1)
            .and_then(|zero_based| zero_based.checked_mul(DERIVED_ADDRESS_BROWSE_PAGE_SIZE))
            .unwrap_or(0);
        let page_end = page_start
            .saturating_add(DERIVED_ADDRESS_BROWSE_PAGE_SIZE)
            .saturating_sub(1);
        let selected_total = state.selected.len();
        let selected_on_page = state.selected_on_page();
        let fully_selected = state.page_fully_selected();

        let prev_root = root.clone();
        let prev_lease = lease.clone();
        let next_root = root.clone();
        let next_lease = lease.clone();
        let go_root = root.clone();
        let go_lease = lease.clone();
        let show_root = root.clone();
        let show_lease = lease.clone();
        let select_all_root = root.clone();
        let clear_root = root.clone();
        let add_root = root.clone();
        let add_lease = lease;

        let mut section = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_2()
            .child(app_muted_text(format!(
                "Derived addresses {page_start}–{page_end} from this wallet (20 per page)."
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button("wallet-public-derive-prev-page", "‹ Prev")
                            .small()
                            .compact()
                            .disabled(loading || page <= 1)
                            .on_click(move |_event, window, cx| {
                                prev_root.update(cx, |root, cx| {
                                    let page = root.public_form.batch.page.saturating_sub(1).max(1);
                                    root.public_form.batch.page = page;
                                    root.public_form.batch.page_input.update(cx, |input, cx| {
                                        input.set_value(page.to_string(), window, cx);
                                    });
                                    root.run_derive_batch_action(
                                        DeriveBatchDeviceAction::PreviewPage,
                                        prev_lease.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(app_muted_text("Page"))
                            .child(
                                div()
                                    .w(gpui::px(64.0))
                                    .child(app_input(&self.public_form.batch.page_input).small()),
                            ),
                    )
                    .child(
                        app_button("wallet-public-derive-goto-page", "Go")
                            .small()
                            .compact()
                            .disabled(loading)
                            .on_click(move |_event, window, cx| {
                                go_root.update(cx, |root, cx| {
                                    root.run_derive_batch_action(
                                        DeriveBatchDeviceAction::PreviewPage,
                                        go_lease.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .child(
                        app_button("wallet-public-derive-next-page", "Next ›")
                            .small()
                            .compact()
                            .disabled(loading)
                            .on_click(move |_event, window, cx| {
                                next_root.update(cx, |root, cx| {
                                    let page = root.public_form.batch.page.saturating_add(1).max(1);
                                    root.public_form.batch.page = page;
                                    root.public_form.batch.page_input.update(cx, |input, cx| {
                                        input.set_value(page.to_string(), window, cx);
                                    });
                                    root.run_derive_batch_action(
                                        DeriveBatchDeviceAction::PreviewPage,
                                        next_lease.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .child(
                        app_button("wallet-public-derive-show-page", "Show")
                            .small()
                            .compact()
                            .disabled(loading)
                            .on_click(move |_event, window, cx| {
                                show_root.update(cx, |root, cx| {
                                    root.run_derive_batch_action(
                                        DeriveBatchDeviceAction::PreviewPage,
                                        show_lease.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    ),
            );

        if state.rows.is_empty() {
            section = section.child(app_muted_text(
                "Enter the vault password, then choose Show to list this page.",
            ));
        } else {
            let mut list = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .h(gpui::px(264.0))
                .overflow_y_scrollbar()
                .p_1();
            for row in &state.rows {
                let row_root = root.clone();
                let index = row.derivation_index;
                let status = if row.already_added {
                    "Already added"
                } else if state.selected.contains(&index) {
                    "Selected"
                } else {
                    "Available"
                };
                list = list.child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap_2()
                        .child(
                            Checkbox::new(SharedString::from(format!(
                                "wallet-public-derive-select-{index}"
                            )))
                            .checked(state.selected.contains(&index))
                            .small()
                            .disabled(row.already_added || loading)
                            .on_click(move |_, _, cx| {
                                row_root.update(cx, |root, cx| {
                                    root.toggle_derived_browse_selection(index, cx);
                                });
                            }),
                        )
                        .child(
                            div()
                                .w(gpui::px(52.0))
                                .flex_none()
                                .child(app_muted_text(format!("#{index}")).text_xs()),
                        )
                        .child(
                            div().flex_1().min_w(gpui::px(0.0)).child(
                                app_strong_text(short_address(&row.address))
                                    .text_xs()
                                    .font_family(APP_MONO_FONT_FAMILY),
                            ),
                        )
                        .child(div().flex_none().child(app_muted_text(status).text_xs())),
                );
            }
            section = section.child(list);
        }

        section = section
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button(
                            "wallet-public-derive-select-all",
                            if fully_selected {
                                "All selected"
                            } else {
                                "Select all"
                            },
                        )
                        .small()
                        .compact()
                        .disabled(loading || state.rows.is_empty() || fully_selected)
                        .on_click(move |_event, _window, cx| {
                            select_all_root.update(cx, |root, cx| {
                                root.select_all_derived_browse_page(cx);
                            });
                        }),
                    )
                    .child(
                        app_button("wallet-public-derive-clear-selection", "Clear")
                            .small()
                            .compact()
                            .disabled(loading || selected_on_page == 0)
                            .on_click(move |_event, _window, cx| {
                                clear_root.update(cx, |root, cx| {
                                    root.clear_derived_browse_page_selection(cx);
                                });
                            }),
                    )
                    .child(app_muted_text(format!("{selected_total} selected")).text_xs()),
            )
            .child(
                app_button(
                    "wallet-public-derive-add-selected",
                    if loading { "Adding…" } else { "Add selected" },
                )
                .primary()
                .small()
                .loading(loading)
                .disabled(loading || authenticating || selected_total == 0)
                .on_click(move |_event, window, cx| {
                    add_root.update(cx, |root, cx| {
                        root.run_derive_batch_action(
                            DeriveBatchDeviceAction::AddSelected,
                            add_lease.clone(),
                            window,
                            cx,
                        );
                    });
                }),
            );

        if let Some(feedback) = self.public_form.batch.feedback.as_ref() {
            section = section.child(app_muted_text(feedback.to_string()).text_xs());
        }
        section
    }

    pub(in crate::root) fn render_derive_range_section(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
        lease: Weak<Cell<bool>>,
    ) -> impl IntoElement {
        let loading = self.public_form.batch.loading || self.public_form.adding_account;
        let authenticating = self.device_auth_in_progress;
        let add_root = root.clone();
        let mut section = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_2()
            .child(app_muted_text(
                "Derive a start index plus count from this wallet (max 100 per batch).",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(gpui::px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(app_muted_text("Start index").text_xs())
                            .child(app_input(&self.public_form.batch.range_start_input).small()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(gpui::px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(app_muted_text("Count").text_xs())
                            .child(app_input(&self.public_form.batch.range_count_input).small()),
                    ),
            )
            .child(
                app_button(
                    "wallet-public-derive-add-range",
                    if loading { "Adding…" } else { "Add range" },
                )
                .primary()
                .small()
                .loading(loading)
                .disabled(loading || authenticating)
                .on_click(move |_event, window, cx| {
                    add_root.update(cx, |root, cx| {
                        root.run_derive_batch_action(
                            DeriveBatchDeviceAction::AddRange,
                            lease.clone(),
                            window,
                            cx,
                        );
                    });
                }),
            );
        if let Some(feedback) = self.public_form.batch.feedback.as_ref() {
            section = section.child(app_muted_text(feedback.to_string()).text_xs());
        }
        section
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_page_parsing_rejects_zero_and_non_numeric() {
        assert_eq!(parse_browse_page("1"), Some(1));
        assert_eq!(parse_browse_page(" 12 "), Some(12));
        assert_eq!(parse_browse_page("0"), None);
        assert_eq!(parse_browse_page(""), None);
        assert_eq!(parse_browse_page("abc"), None);
        assert_eq!(parse_browse_page("1.5"), None);
    }

    #[test]
    fn range_parsing_validates_start_and_count() {
        assert_eq!(parse_range_inputs("100", "5"), Ok((100, 5)));
        assert!(parse_range_inputs("-1", "5").is_err());
        assert!(parse_range_inputs("abc", "5").is_err());
        assert!(parse_range_inputs("100", "0").is_err());
        assert!(parse_range_inputs("100", "101").is_err());
        assert!(parse_range_inputs("100", "abc").is_err());
    }
}
