use crate::theme;
use chrono::{Local, Timelike};
use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window};
use std::time::Duration;
use ztheme::Theme;

pub struct ClockModule {
    text: String,
}

impl ClockModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let text = Local::now().format("%H:%M").to_string();
        cx.spawn(async move |this, cx| loop {
            let now = Local::now();
            let secs_until_next_min = (60u64).saturating_sub(now.second() as u64).max(1);
            cx.background_executor()
                .timer(Duration::from_secs(secs_until_next_min))
                .await;
            let new_text = Local::now().format("%H:%M").to_string();
            if this
                .update(cx, |m, cx| {
                    if m.text != new_text {
                        m.text = new_text;
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        ClockModule { text }
    }
}

impl Render for ClockModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.global::<Theme>();
        theme::pill(cx)
            .text_color(t.fg)
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(self.text.clone())
    }
}
