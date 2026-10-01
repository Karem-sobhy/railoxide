//! Touch ID as an alternative to typing the vault password.
//!
//! Every prompt keeps its password field. When Touch ID unlock is on, a Touch
//! ID icon inside the field reads the sealed vault password and hands it to
//! the same submit path a typed password takes, so every vault check still runs
//! against the password itself.

use std::{
    cell::Cell,
    rc::{Rc, Weak},
    sync::Arc,
};

use gpui::{
    AppContext as _, Context, ElementId, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Styled as _, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::ButtonVariants as _,
    dialog::Cancel,
    input::{InputEvent, InputGroupAddon, InputGroupAddonAlignment, InputGroupButton, InputState},
    notification::Notification,
};
use tokio::runtime::Handle;
use ui::controls::{
    app_button, app_input, app_input_group, app_masked_input, app_muted_text, app_strong_text,
};
use ui::theme;
use wallet_ops::biometric::{BiometricError, biometric_unlock_supported};
use wallet_ops::vault::{BiometricUnlockStatus, DesktopVaultStore, VaultError};
use zeroize::Zeroizing;

use super::actions::TOUCH_ID_BUTTON_KEY_CONTEXT;
use super::{VaultState, WalletRoot, new_masked_input, secondary_dialog_content_width};
use crate::assets::RailgunActionIcon;

const ENABLE_TOUCH_ID_DIALOG_WIDTH: Pixels = px(420.0);
const TOUCH_ID_FAILED: &str = "Touch ID failed. Enter the vault password instead.";

#[cfg(test)]
mod tests;

/// Each reason finishes the "… is trying to" sentence of the system Touch ID prompt.
pub(in crate::root) const TOUCH_ID_REASON_UNLOCK: &str = "unlock the wallet vault";
pub(in crate::root) const TOUCH_ID_REASON_SPEND: &str = "authorize this action";
pub(in crate::root) const TOUCH_ID_REASON_KEY_EXPORT: &str = "reveal wallet keys";
pub(in crate::root) const TOUCH_ID_REASON_CHANGE_PASSWORD: &str = "change the vault password";
pub(in crate::root) const TOUCH_ID_REASON_ADD_WALLET: &str = "add a wallet";
pub(in crate::root) const TOUCH_ID_REASON_PUBLIC_ACCOUNT: &str = "add a public account";
pub(in crate::root) const TOUCH_ID_REASON_PASSPHRASE_WALLET: &str = "open a passphrase wallet";

/// Reads the sealed vault password behind a Touch ID prompt.
#[derive(Clone)]
pub(in crate::root) struct TouchIdPrompt {
    runtime: Handle,
    store: Arc<DesktopVaultStore>,
}

pub(in crate::root) enum TouchIdPassword {
    Password(Zeroizing<String>),
    /// The prompt was dismissed; leave the form as it was.
    Cancelled,
    Failed(Arc<str>),
}

impl TouchIdPrompt {
    /// Shows the system Touch ID prompt off the UI thread and passes the
    /// outcome to `on_done` on the view that asked for it.
    pub(in crate::root) fn run<V: 'static>(
        &self,
        reason: &'static str,
        window: &Window,
        cx: &Context<'_, V>,
        on_done: impl FnOnce(&mut V, TouchIdPassword, &mut Window, &mut Context<'_, V>) + 'static,
    ) {
        let store = Arc::clone(&self.store);
        let join = self
            .runtime
            .spawn_blocking(move || store.biometric_vault_password(reason));
        cx.spawn_in(window, async move |this, cx| {
            let outcome = match join.await {
                Ok(Ok(password)) => TouchIdPassword::Password(password),
                Ok(Err(error)) => touch_id_failure(&error),
                Err(error) => {
                    tracing::warn!(%error, "Touch ID task failed");
                    TouchIdPassword::Failed(Arc::from(TOUCH_ID_FAILED))
                }
            };
            let _ = this.update_in(cx, |view, window, cx| on_done(view, outcome, window, cx));
        })
        .detach();
    }
}

fn touch_id_failure(error: &VaultError) -> TouchIdPassword {
    match error {
        VaultError::Biometric(BiometricError::Cancelled) => TouchIdPassword::Cancelled,
        VaultError::Biometric(BiometricError::LockedOut) => {
            TouchIdPassword::Failed(Arc::from(error.to_string()))
        }
        VaultError::Biometric(BiometricError::Unavailable) => TouchIdPassword::Failed(Arc::from(
            "Touch ID is not available right now. Enter the vault password instead.",
        )),
        VaultError::BiometricUnlockDisabled => TouchIdPassword::Failed(Arc::from(
            "Touch ID needs the vault password once. Enter it to continue.",
        )),
        error => {
            tracing::warn!(%error, "Touch ID unlock failed");
            TouchIdPassword::Failed(Arc::from(TOUCH_ID_FAILED))
        }
    }
}

/// An input-group icon that asks for Touch ID instead of the typed vault password.
pub(in crate::root) fn touch_id_button(
    id: impl Into<ElementId>,
    label: &'static str,
    pending: bool,
    disabled: bool,
) -> InputGroupButton {
    InputGroupButton::new(id)
        .key_context(TOUCH_ID_BUTTON_KEY_CONTEXT)
        .icon(Icon::new(RailgunActionIcon::Fingerprint))
        .accessibility_label(label)
        .tooltip("Use Touch ID instead of the vault password")
        .loading(pending)
        .disabled(disabled || pending)
}

/// A vault password field with an optional Touch ID icon inside it.
pub(in crate::root) fn masked_input_with_touch_id(
    input: &Entity<InputState>,
    disabled: bool,
    touch_id: Option<InputGroupButton>,
) -> gpui::Div {
    match touch_id {
        Some(button) => div().w_full().child(
            app_input_group(
                ("vault-password-touch-id", input.entity_id()),
                input,
                "Vault password",
            )
            .input(
                app_input(input)
                    .role(gpui::accesskit::Role::PasswordInput)
                    .bg(gpui::transparent_black()),
            )
            .disabled(disabled)
            .addon(
                InputGroupAddon::new(("vault-password-touch-id-actions", input.entity_id()))
                    .align(InputGroupAddonAlignment::InlineEnd)
                    .child(button),
            ),
        ),
        None => app_masked_input(input, disabled),
    }
}

impl WalletRoot {
    pub(in crate::root) fn refresh_touch_id_status(&mut self) {
        self.touch_id_supported = biometric_unlock_supported();
        self.touch_id_status = self
            .vault_store
            .as_ref()
            .and_then(|store| {
                store
                    .biometric_unlock_status()
                    .inspect_err(|error| tracing::warn!(%error, "failed to read Touch ID status"))
                    .ok()
            })
            .unwrap_or(BiometricUnlockStatus::Disabled);
    }

    /// Returns a prompt when this device supports the saved Touch ID enrollment.
    /// Temporary macOS lockout is reported when the user attempts authentication.
    pub(in crate::root) fn touch_id_prompt(&mut self) -> Option<TouchIdPrompt> {
        self.refresh_touch_id_status();
        self.touch_id_prompt_cached()
    }

    /// Like [`Self::touch_id_prompt`] without re-reading the status, for render.
    pub(in crate::root) fn touch_id_prompt_cached(&self) -> Option<TouchIdPrompt> {
        if !self.touch_id_supported || self.touch_id_status != BiometricUnlockStatus::Enabled {
            return None;
        }
        Some(TouchIdPrompt {
            runtime: self.runtime.clone(),
            store: Arc::clone(self.vault_store.as_ref()?),
        })
    }

    /// The saved enrollment state for the Touch ID setting, when supported.
    pub(in crate::root) const fn touch_id_setting_state(&self) -> Option<bool> {
        match self.touch_id_status {
            BiometricUnlockStatus::Enabled | BiometricUnlockStatus::NeedsReenrollment => Some(true),
            BiometricUnlockStatus::Disabled if self.touch_id_supported => Some(false),
            BiometricUnlockStatus::Disabled => None,
        }
    }

    /// Reseals a stale Touch ID record with a password that was just verified.
    /// Runs on the caller's blocking thread.
    pub(in crate::root) fn renew_touch_id(store: &DesktopVaultStore, password: &str) {
        if let Err(error) = store.renew_biometric_unlock(password) {
            tracing::warn!(%error, "failed to renew Touch ID unlock");
        }
    }

    /// Offers Touch ID once when the app starts on the unlock screen. Locking
    /// later does not prompt, so the system dialog never waits on an idle Mac.
    pub(in crate::root) fn prompt_touch_id_on_unlock_screen_if_requested(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if std::mem::take(&mut self.touch_id_unlock_on_render)
            && matches!(self.vault_state, VaultState::UnlockVault)
        {
            cx.defer_in(window, |root, window, cx| {
                if matches!(root.vault_state, VaultState::UnlockVault) {
                    root.unlock_vault_with_touch_id(window, cx);
                }
            });
        }
    }

    pub(in crate::root) fn unlock_vault_with_touch_id(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.unlock_in_progress || self.touch_id_in_progress {
            return;
        }
        let Some(prompt) = self.touch_id_prompt() else {
            cx.notify();
            return;
        };
        self.touch_id_in_progress = true;
        self.vault_error = None;
        let generation = self.active_wallet_generation;
        cx.notify();
        prompt.run(
            TOUCH_ID_REASON_UNLOCK,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_vault_touch_id_unlock(generation, outcome, window, cx);
            },
        );
    }

    fn finish_vault_touch_id_unlock(
        &mut self,
        generation: u64,
        outcome: TouchIdPassword,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.touch_id_in_progress = false;
        if matches!(&outcome, TouchIdPassword::Failed(_)) {
            self.refresh_touch_id_status();
        }
        cx.notify();
        if self.active_wallet_generation != generation
            || !matches!(self.vault_state, VaultState::UnlockVault)
        {
            return;
        }
        match outcome {
            TouchIdPassword::Password(password) => {
                self.unlock_vault_with_password(password, None, window, cx);
            }
            TouchIdPassword::Cancelled => {
                self.focus_vault_input_on_render = true;
            }
            TouchIdPassword::Failed(message) => {
                self.focus_vault_input_on_render = true;
                self.vault_error = Some(message);
            }
        }
    }

    /// Lets Touch ID stand in for the add-wallet password field. The user still
    /// picks the action, such as confirming a saved recovery phrase.
    pub(in crate::root) fn authorize_add_wallet_with_touch_id(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.touch_id_in_progress
            || !self
                .add_wallet_dialog_lease
                .upgrade()
                .is_some_and(|open| open.get())
        {
            return;
        }
        let Some(prompt) = self.touch_id_prompt() else {
            cx.notify();
            return;
        };
        self.touch_id_in_progress = true;
        self.vault_error = None;
        let lease = self.add_wallet_dialog_lease.clone();
        let generation = self.active_wallet_generation;
        cx.notify();
        prompt.run(
            TOUCH_ID_REASON_ADD_WALLET,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_add_wallet_touch_id(&lease, generation, outcome, window, cx);
            },
        );
    }

    fn finish_add_wallet_touch_id(
        &mut self,
        lease: &Weak<Cell<bool>>,
        generation: u64,
        outcome: TouchIdPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.touch_id_in_progress = false;
        if matches!(&outcome, TouchIdPassword::Failed(_)) {
            self.refresh_touch_id_status();
        }
        cx.notify();
        if !lease.upgrade().is_some_and(|open| open.get())
            || self.active_wallet_generation != generation
            || !matches!(self.vault_state, VaultState::ViewUnlocked)
        {
            return;
        }
        match outcome {
            TouchIdPassword::Password(password) => {
                self.add_wallet_password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.add_wallet_touch_id_password = Some(password);
            }
            TouchIdPassword::Cancelled => {}
            TouchIdPassword::Failed(message) => {
                self.vault_error = Some(message);
            }
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn unlock_hardware_profile_with_touch_id(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.touch_id_in_progress
            || self.hardware_profile_unlock.in_progress
            || !self
                .hardware_profile_unlock
                .dialog_lease
                .upgrade()
                .is_some_and(|open| open.get())
        {
            return;
        }
        let Some(prompt) = self.touch_id_prompt() else {
            cx.notify();
            return;
        };
        self.touch_id_in_progress = true;
        self.hardware_profile_unlock.error = None;
        let generation = self.hardware_wallet_creation_generation;
        let lease = self.hardware_profile_unlock.dialog_lease.clone();
        cx.notify();
        prompt.run(
            TOUCH_ID_REASON_UNLOCK,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_hardware_profile_touch_id_unlock(
                    &lease, generation, outcome, window, cx,
                );
            },
        );
    }

    #[cfg(feature = "hardware")]
    fn finish_hardware_profile_touch_id_unlock(
        &mut self,
        lease: &Weak<Cell<bool>>,
        generation: u64,
        outcome: TouchIdPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.touch_id_in_progress = false;
        if matches!(&outcome, TouchIdPassword::Failed(_)) {
            self.refresh_touch_id_status();
        }
        cx.notify();
        if !lease.upgrade().is_some_and(|open| open.get())
            || self.hardware_wallet_creation_generation != generation
        {
            return;
        }
        match outcome {
            TouchIdPassword::Password(password) => {
                if self.hardware_profile_unlock_requires_password()
                    && !self.hardware_profile_unlock.in_progress
                {
                    self.hardware_profile_touch_id_password = Some(password);
                    self.unlock_hardware_profile_from_dialog(window, cx);
                    self.hardware_profile_touch_id_password = None;
                }
            }
            TouchIdPassword::Cancelled => {}
            TouchIdPassword::Failed(message) => {
                self.hardware_profile_unlock.error = Some(message);
            }
        }
    }

    pub(in crate::root) fn clear_add_wallet_password(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.add_wallet_touch_id_password = None;
        self.add_wallet_password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    /// The add-wallet password field, or a note that Touch ID supplied it.
    pub(in crate::root) fn render_add_wallet_password(
        &self,
        root: Entity<Self>,
        disabled: bool,
    ) -> gpui::Div {
        if self.add_wallet_touch_id_password.is_some() {
            return div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .px(px(10.0))
                .py(px(6.0))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    Icon::new(RailgunActionIcon::Fingerprint)
                        .size_4()
                        .text_color(rgb(theme::SUCCESS)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(app_muted_text("Vault password provided by Touch ID")),
                )
                .child(
                    app_button("add-wallet-touch-id-clear", "Type instead")
                        .ghost()
                        .xsmall()
                        .flex_none()
                        .disabled(disabled)
                        .on_click(move |_event, window, cx| {
                            root.update(cx, |root, cx| {
                                root.clear_add_wallet_password(window, cx);
                                cx.notify();
                            });
                        }),
                );
        }
        masked_input_with_touch_id(
            &self.add_wallet_password_input,
            disabled || self.touch_id_in_progress,
            self.touch_id_prompt_cached().is_some().then(|| {
                touch_id_button(
                    "add-wallet-touch-id",
                    "Touch ID",
                    self.touch_id_in_progress,
                    disabled,
                )
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.authorize_add_wallet_with_touch_id(window, cx);
                    });
                })
            }),
        )
    }

    pub(in crate::root) fn disable_touch_id(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.as_ref() else {
            return;
        };
        match store.disable_biometric_unlock() {
            Ok(()) => {
                window.push_notification(Notification::success("Touch ID unlock turned off."), cx);
            }
            Err(error) => {
                tracing::warn!(%error, "failed to turn off Touch ID unlock");
                window.push_notification(
                    Notification::error(format!("Failed to turn off Touch ID: {error}")),
                    cx,
                );
            }
        }
        self.refresh_touch_id_status();
        cx.notify();
    }

    pub(in crate::root) fn open_enable_touch_id_dialog(
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        EnableTouchIdDialogContent::open(window, cx);
    }

    /// Seals the password that just created the vault when the user opted in.
    pub(in crate::root) fn enable_touch_id_after_vault_creation(
        &self,
        password: &Zeroizing<String>,
        cx: &Context<'_, Self>,
    ) {
        if !(self.touch_id_supported && self.enable_touch_id_on_create) {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let password = password.clone();
        let join = self
            .runtime
            .spawn_blocking(move || store.enable_biometric_unlock(password.as_str()));
        cx.spawn(async move |this, cx| {
            match join.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "failed to turn on Touch ID for the new vault");
                }
                Err(error) => tracing::warn!(%error, "turn on Touch ID task failed"),
            }
            let _ = this.update(cx, |root, cx| {
                root.refresh_touch_id_status();
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root) fn set_enable_touch_id_on_create(
        &mut self,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.enable_touch_id_on_create = enabled;
        cx.notify();
    }
}

struct EnableTouchIdDialogContent {
    root: Entity<WalletRoot>,
    password_input: Entity<InputState>,
    error: Option<Arc<str>>,
    pending: bool,
    lease: Weak<Cell<bool>>,
    dialog_focus: Option<FocusHandle>,
}

impl EnableTouchIdDialogContent {
    fn open(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Entity<Self> {
        let root = cx.entity();
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        let content = cx.new(|cx| Self::new(root, identity, window, cx));
        let dialog_content = content.clone();
        let dialog_width = (window.viewport_size().width * 0.92).min(ENABLE_TOUCH_ID_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let identity = Rc::downgrade(&lease);
            let cancel_content = dialog_content.clone();
            let pending = dialog_content.read(cx).pending;
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, _, cx| !cancel_content.read(cx).pending)
                .on_close(move |_, _, _| {
                    if let Some(lease) = identity.upgrade() {
                        lease.set(false);
                    }
                })
                .close_button(!pending)
                .overlay_closable(!pending)
                .title(app_strong_text("Turn on Touch ID"))
                .child(div().w(content_width).child(dialog_content.clone()))
        });
        content.update(cx, |content, cx| content.dialog_focus = window.focused(cx));
        let focus_content = content.clone();
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
        content
    }

    fn new(
        root: Entity<WalletRoot>,
        lease: Weak<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let password_input = new_masked_input(window, cx, "vault password");
        cx.subscribe_in(
            &password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        Self {
            root,
            password_input,
            error: None,
            pending: false,
            lease,
            dialog_focus: None,
        }
    }

    fn focus_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending {
            return;
        }
        let password = Zeroizing::new(self.password_input.read(cx).value().to_string());
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if password.trim().is_empty() {
            self.error = Some(Arc::from("Enter the vault password to turn on Touch ID"));
            cx.notify();
            return;
        }
        let root = self.root.read(cx);
        let Some(store) = root.vault_store.clone() else {
            self.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let join = root
            .runtime
            .spawn_blocking(move || store.enable_biometric_unlock(password.as_str()));
        self.observe_enrollment(
            async move {
                match join.await {
                    Ok(result) => result.map_err(|error| enable_touch_id_error_message(&error)),
                    Err(error) => {
                        tracing::warn!(%error, "turn on Touch ID task failed");
                        Err(Arc::from("Failed to turn on Touch ID. Try again."))
                    }
                }
            },
            window,
            cx,
        );
    }

    fn observe_enrollment(
        &mut self,
        completion: impl Future<Output = Result<(), Arc<str>>> + 'static,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending = true;
        self.error = None;
        let root = self.root.downgrade();
        let lease = self.lease.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = completion.await;
            // Enrollment commits in the worker even if locking forcibly closes its dialog.
            let _ = root.update(cx, |root, cx| {
                root.refresh_touch_id_status();
                cx.notify();
            });
            let original_open = lease.upgrade().is_some_and(|open| open.get());
            let _ = cx.update(|window, cx| match &result {
                Ok(()) => window.push_notification(
                    Notification::success(
                        "Touch ID unlock turned on. The vault password still works.",
                    ),
                    cx,
                ),
                Err(error) if !original_open => {
                    window.push_notification(Notification::error(error.to_string()), cx);
                }
                Err(_) => {}
            });
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.pending = false;
                cx.notify();
                if !original_open {
                    return;
                }
                let focused = dialog
                    .dialog_focus
                    .as_ref()
                    .is_some_and(|focus| focus.contains_focused(window, cx));
                match result {
                    Ok(()) => {
                        if focused {
                            window.close_dialog(cx);
                        }
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                        if focused {
                            dialog.focus_password(window, cx);
                        }
                    }
                }
            });
        })
        .detach();
    }
}

fn enable_touch_id_error_message(error: &VaultError) -> Arc<str> {
    match error {
        VaultError::UnlockFailed => {
            Arc::from("Password did not unlock the vault. Check it and try again.")
        }
        VaultError::Biometric(BiometricError::Unavailable) => {
            Arc::from("Touch ID is not available on this Mac right now.")
        }
        VaultError::Biometric(BiometricError::LockedOut) => Arc::from(error.to_string()),
        error => {
            tracing::warn!(%error, "failed to turn on Touch ID unlock");
            Arc::from(format!("Failed to turn on Touch ID: {error}"))
        }
    }
}

impl Render for EnableTouchIdDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                app_muted_text(
                    "Enter the vault password to use Touch ID wherever the wallet asks for it. The password stays sealed in this Mac's Secure Enclave and only opens with a fingerprint enrolled now. Typing the password keeps working.",
                )
                .whitespace_normal(),
            )
            .child(app_masked_input(&self.password_input, self.pending))
            .when_some(self.error.as_ref(), |this, error| {
                this.child(
                    app_muted_text(error.to_string())
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                )
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("wallet-enable-touch-id-cancel", "Cancel")
                            .debug_selector(|| "wallet-enable-touch-id-cancel".into())
                            .flex_none()
                            .disabled(self.pending)
                            .on_click(move |_event, window, cx| {
                                window.dispatch_action(Box::new(Cancel), cx);
                            }),
                    )
                    .child(
                        app_button("wallet-enable-touch-id-submit", "Turn on")
                            .primary()
                            .flex_none()
                            .loading(self.pending)
                            .disabled(self.pending)
                            .on_click(move |_event, window, cx| {
                                dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                            }),
                    ),
            )
    }
}
