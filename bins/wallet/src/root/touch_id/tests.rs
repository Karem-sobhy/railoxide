use super::*;
use gpui::TestAppContext;

struct DialogWindow;

impl Render for DialogWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .children(crate::root::startup::render_wallet_overlay_layers(
                window, cx,
            ))
    }
}

#[gpui::test]
fn touch_id_enrollment_vetoes_cancel_until_completion(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        root = Some(crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        ));
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));

    for result in [Err(Arc::from("enrollment failed")), Ok(())] {
        let dialog = cx.update(|window, cx| {
            root.update(cx, |_, cx| EnableTouchIdDialogContent::open(window, cx))
        });
        cx.run_until_parked();
        let (send, receive) = tokio::sync::oneshot::channel();
        cx.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.observe_enrollment(async move { receive.await.unwrap() }, window, cx);
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let cancel = cx.debug_bounds("wallet-enable-touch-id-cancel").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("escape");
        cx.simulate_click(gpui::point(px(5.), px(5.)), gpui::Modifiers::none());
        cx.update(|window, cx| assert!(window.has_active_dialog(cx)));

        let failed = result.is_err();
        send.send(result).unwrap();
        cx.run_until_parked();
        assert!(!dialog.read_with(cx, |dialog, _| dialog.pending));
        cx.update(|window, cx| assert_eq!(window.has_active_dialog(cx), failed));
        if failed {
            assert!(dialog.read_with(cx, |dialog, _| dialog.error.is_some()));
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let cancel = cx.debug_bounds("wallet-enable-touch-id-cancel").unwrap();
            cx.simulate_click(cancel.center(), gpui::Modifiers::none());
            cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
        }
    }
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn touch_id_enrollment_refreshes_root_without_closing_or_focusing_a_newer_dialog(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        root = Some(crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        ));
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));

    for (replace, retain, result) in [
        (true, false, Err(Arc::from("enrollment failed"))),
        (true, true, Ok(())),
        (false, true, Ok(())),
        (false, true, Err(Arc::from("enrollment failed"))),
    ] {
        let dialog = cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                // Deliberately stale cache: completion must reread the disabled test vault.
                root.touch_id_status = BiometricUnlockStatus::Enabled;
                EnableTouchIdDialogContent::open(window, cx)
            })
        });
        cx.run_until_parked();
        let (send, receive) = tokio::sync::oneshot::channel();
        let replacement_focus = cx.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.observe_enrollment(async move { receive.await.unwrap() }, window, cx);
            });
            if replace {
                window.close_all_dialogs(cx);
            }
            window.open_dialog(cx, |dialog, _, _| dialog.title("Newer dialog"));
            window.focused(cx).unwrap()
        });
        let old = dialog.downgrade();
        // Keep one closed entity alive to prove that entity lifetime is not dialog lifetime.
        let retained = retain.then_some(dialog);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        if !retain {
            assert!(old.upgrade().is_none());
        }
        send.send(result).unwrap();
        cx.run_until_parked();
        assert_eq!(
            root.read_with(cx, |root, _| root.touch_id_status),
            BiometricUnlockStatus::Disabled
        );
        cx.update(|window, cx| {
            assert!(window.has_active_dialog(cx));
            assert!(replacement_focus.is_focused(window));
            window.close_dialog(cx);
            assert_eq!(window.has_active_dialog(cx), !replace);
            window.close_all_dialogs(cx);
        });
        drop(retained);
    }
    cx.update(|window, _| window.remove_window());
}
