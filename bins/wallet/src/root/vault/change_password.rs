use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, WindowExt,
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
};
use ui::controls::{app_button, app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::vault::VaultError;
use zeroize::Zeroizing;

use super::super::touch_id::{
    TOUCH_ID_REASON_CHANGE_PASSWORD, TouchIdPassword, TouchIdPrompt, masked_input_with_touch_id,
    touch_id_button,
};
use super::super::{WalletRoot, new_masked_input, secondary_dialog_content_width};
use super::VaultState;

const CHANGE_VAULT_PASSWORD_DIALOG_WIDTH: gpui::Pixels = px(460.0);

struct ChangeVaultPasswordDialogContent {
    root: Entity<WalletRoot>,
    current_password_input: Entity<InputState>,
    new_password_input: Entity<InputState>,
    confirm_password_input: Entity<InputState>,
    pending: bool,
    error: Option<Arc<str>>,
    touch_id: Option<TouchIdPrompt>,
    touch_id_pending: bool,
}

#[derive(Clone, Copy)]
enum ChangeVaultPasswordEnterAction {
    FocusNewPassword,
    FocusConfirmPassword,
    Submit,
}

impl ChangeVaultPasswordDialogContent {
    fn new(
        root: Entity<WalletRoot>,
        touch_id: Option<TouchIdPrompt>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let current_password_input = new_masked_input(window, cx, "current vault password");
        let new_password_input = new_masked_input(window, cx, "new vault password");
        let confirm_password_input = new_masked_input(window, cx, "confirm new vault password");
        for (input, enter_action) in [
            (
                current_password_input.clone(),
                ChangeVaultPasswordEnterAction::FocusNewPassword,
            ),
            (
                new_password_input.clone(),
                ChangeVaultPasswordEnterAction::FocusConfirmPassword,
            ),
            (
                confirm_password_input.clone(),
                ChangeVaultPasswordEnterAction::Submit,
            ),
        ] {
            cx.subscribe_in(
                &input,
                window,
                move |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        this.handle_enter(enter_action, window, cx);
                    }
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }

        Self {
            root,
            current_password_input,
            new_password_input,
            confirm_password_input,
            pending: false,
            error: None,
            touch_id,
            touch_id_pending: false,
        }
    }

    fn focus_current_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.current_password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn handle_enter(
        &mut self,
        enter_action: ChangeVaultPasswordEnterAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending || self.touch_id_pending {
            return;
        }
        match enter_action {
            ChangeVaultPasswordEnterAction::FocusNewPassword => {
                self.new_password_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            ChangeVaultPasswordEnterAction::FocusConfirmPassword => {
                self.confirm_password_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            ChangeVaultPasswordEnterAction::Submit => self.submit(window, cx),
        }
    }

    fn submit(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if self.pending || self.touch_id_pending {
            return;
        }
        let current_password =
            Zeroizing::new(self.current_password_input.read(cx).value().to_string());
        if current_password.trim().is_empty() {
            self.error = Some(Arc::from("Enter the current vault password"));
            cx.notify();
            return;
        }
        self.submit_with_current_password(current_password, window, cx);
    }

    /// Uses Touch ID for the current password once the new password is valid.
    fn submit_with_touch_id(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if self.pending || self.touch_id_pending {
            return;
        }
        let Some(prompt) = self.touch_id.clone() else {
            return;
        };
        if let Err(message) = self.new_password(cx) {
            self.error = Some(message);
            cx.notify();
            return;
        }
        self.touch_id_pending = true;
        self.error = None;
        cx.notify();
        prompt.run(
            TOUCH_ID_REASON_CHANGE_PASSWORD,
            window,
            cx,
            |dialog, outcome, window, cx| {
                dialog.touch_id_pending = false;
                match outcome {
                    TouchIdPassword::Password(current_password) => {
                        dialog.submit_with_current_password(current_password, window, cx);
                    }
                    TouchIdPassword::Cancelled => {}
                    TouchIdPassword::Failed(message) => {
                        dialog.touch_id = None;
                        dialog.error = Some(message);
                        dialog.focus_current_password(window, cx);
                    }
                }
                cx.notify();
            },
        );
    }

    fn new_password(&self, cx: &Context<'_, Self>) -> Result<Zeroizing<String>, Arc<str>> {
        let new_password = Zeroizing::new(self.new_password_input.read(cx).value().to_string());
        let confirm_password =
            Zeroizing::new(self.confirm_password_input.read(cx).value().to_string());
        if new_password.trim().is_empty() {
            return Err(Arc::from("Enter a new vault password"));
        }
        if new_password.as_str() != confirm_password.as_str() {
            return Err(Arc::from("New vault passwords do not match"));
        }
        Ok(new_password)
    }

    fn submit_with_current_password(
        &mut self,
        current_password: Zeroizing<String>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let new_password = match self.new_password(cx) {
            Ok(new_password) => new_password,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        if current_password.as_str() == new_password.as_str() {
            self.error = Some(Arc::from(
                "Choose a new password that is different from the current password",
            ));
            cx.notify();
            return;
        }

        let start = self.root.update(cx, move |root, _cx| {
            let Some(store) = root.vault_store.clone() else {
                return Err(Arc::from("Wallet vault storage is unavailable"));
            };
            Ok(root.runtime.spawn_blocking(move || {
                store.reencrypt_vault(current_password.as_str(), new_password.as_str())
            }))
        });
        let join = match start {
            Ok(join) => join,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };

        self.pending = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.pending = false;
                match result {
                    Ok(Ok(())) => {
                        dialog.clear_inputs(window, cx);
                        let root = dialog.root.clone();
                        root.update(cx, |root, cx| {
                            root.clear_spend_authorization(cx);
                            root.refresh_touch_id_status();
                        });
                        window.close_dialog(cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "vault password change failed");
                        dialog.error = Some(change_vault_password_error_message(&error));
                        cx.notify();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "vault password change task failed");
                        dialog.error = Some(Arc::from(
                            "Failed to change the vault password. See logs for diagnostics.",
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn clear_inputs(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        for input in [
            &self.current_password_input,
            &self.new_password_input,
            &self.confirm_password_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }
}

impl Render for ChangeVaultPasswordDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let touch_id_dialog = dialog.clone();
        let busy = self.pending || self.touch_id_pending;
        let touch_id = self.touch_id.is_some().then(|| {
            touch_id_button(
                "wallet-change-vault-password-touch-id",
                "Touch ID",
                self.touch_id_pending,
                self.pending,
            )
            .on_click(move |_event, window, cx| {
                touch_id_dialog.update(cx, |dialog, cx| dialog.submit_with_touch_id(window, cx));
            })
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(password_field(
                "Current password",
                &self.current_password_input,
                busy,
                touch_id,
            ))
            .child(password_field(
                "New password",
                &self.new_password_input,
                busy,
                None,
            ))
            .child(password_field(
                "Confirm new password",
                &self.confirm_password_input,
                busy,
                None,
            ))
            .when(self.touch_id.is_some(), |this| {
                this.child(
                    app_muted_text(
                        "Enter the new password twice, then use Touch ID or the current password to confirm.",
                    )
                    .text_xs()
                    .whitespace_normal(),
                )
            })
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
                        app_button("wallet-change-vault-password-cancel", "Cancel")
                            .disabled(self.pending)
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        app_button(
                            "wallet-change-vault-password-submit",
                            if self.pending {
                                "Changing..."
                            } else {
                                "Change password"
                            },
                        )
                        .primary()
                        .disabled(busy)
                        .on_click(move |_event, window, cx| {
                            dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                        }),
                    ),
            )
    }
}

impl WalletRoot {
    pub(in crate::root) fn open_change_vault_password_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
            self.set_vault_error("Unlock the wallet vault before changing its password", cx);
            return;
        }
        window.close_all_dialogs(cx);
        let root = cx.entity();
        let touch_id = self.touch_id_prompt();
        let content =
            cx.new(|cx| ChangeVaultPasswordDialogContent::new(root, touch_id, window, cx));
        let focus_content = content.clone();
        let dialog_width =
            (window.viewport_size().width * 0.92).min(CHANGE_VAULT_PASSWORD_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .title(app_strong_text("Change vault password"))
                .child(div().w(content_width).child(content.clone()))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| {
                content.focus_current_password(window, cx);
            });
        });
    }
}

fn password_field(
    label: &'static str,
    input: &Entity<InputState>,
    disabled: bool,
    touch_id: Option<Button>,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label))
        .child(masked_input_with_touch_id(input, disabled, touch_id))
}

fn change_vault_password_error_message(error: &VaultError) -> Arc<str> {
    match error {
        VaultError::UnlockFailed => {
            Arc::from("Current password did not unlock the vault. Check it and try again.")
        }
        VaultError::VaultNotFound => Arc::from("Wallet vault storage was not found."),
        _ => Arc::from(format!("Failed to change vault password: {error}")),
    }
}
