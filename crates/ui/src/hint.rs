use gpui::{
    Div, FontWeight, ParentElement as _, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, rems, rgb,
};

use crate::{broadcaster_picker::broadcaster_picker_status_tooltip_width, theme};

/// An explanation card for hint tooltips and popovers, capped to the viewport. The title takes
/// `title_color`, a theme colour; callers add the paragraphs.
#[must_use]
pub fn hint_card(title: impl Into<SharedString>, title_color: u32, window: &Window) -> Div {
    div()
        .w(rems(22.5))
        .when_some(
            broadcaster_picker_status_tooltip_width(
                window.viewport_size().width,
                window.rem_size(),
            ),
            Styled::max_w,
        )
        .p(rems(0.75))
        .flex()
        .flex_col()
        .gap_2()
        .text_size(rems(0.75))
        .text_color(rgb(theme::TEXT))
        .child(
            div()
                .text_color(rgb(title_color))
                .font_weight(FontWeight::MEDIUM)
                .child(title.into()),
        )
}
