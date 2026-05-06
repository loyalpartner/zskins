use crate::theme;
use gpui::{div, Context, IntoElement, ParentElement, Render, Styled, Window};
use std::fs;
use std::time::Duration;
use ztheme::Theme;

pub struct BrightnessModule {
    percent: Option<u32>,
    device: Option<String>,
    max_brightness: u32,
}

impl BrightnessModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let device = find_backlight();
        let max_brightness = device
            .as_deref()
            .and_then(|d| {
                fs::read_to_string(format!("/sys/class/backlight/{d}/max_brightness")).ok()
            })
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let percent = read_brightness(device.as_deref(), max_brightness);

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let cached = this.read_with(cx, |m, _| (m.device.clone(), m.max_brightness));
            let Ok((dev, max)) = cached else { break };
            let pct = read_brightness(dev.as_deref(), max);
            if this
                .update(cx, |m, cx| {
                    if m.percent != pct {
                        m.percent = pct;
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        BrightnessModule {
            percent,
            device,
            max_brightness,
        }
    }
}

fn find_backlight() -> Option<String> {
    fs::read_dir("/sys/class/backlight")
        .ok()?
        .flatten()
        .next()
        .map(|e| e.file_name().to_string_lossy().to_string())
}

fn read_brightness(device: Option<&str>, max: u32) -> Option<u32> {
    if max == 0 {
        return None;
    }
    let name = device?;
    let cur: u32 = fs::read_to_string(format!("/sys/class/backlight/{name}/brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((cur * 100) / max)
}

impl Render for BrightnessModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pct) = self.percent else {
            return div();
        };
        let icon = match pct {
            0..=30 => "󰃞",
            31..=70 => "󰃟",
            _ => "󰃠",
        };
        let t = cx.global::<Theme>();
        theme::pill(cx)
            .bg(gpui::Hsla::transparent_black())
            .flex()
            .items_center()
            .gap_0p5()
            .child(div().text_color(t.accent).child(icon.to_string()))
            .child(theme::pct_label(pct, t.fg_dim))
    }
}
