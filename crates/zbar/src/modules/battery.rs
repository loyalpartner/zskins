use crate::theme;
use gpui::{div, Context, IntoElement, ParentElement, Render, Styled, Window};
use std::fs;
use std::time::Duration;
use ztheme::Theme;

pub struct BatteryModule {
    capacity: Option<u8>,
    status: BatteryStatus,
    device: Option<String>,
}

#[derive(Default, PartialEq, Clone)]
enum BatteryStatus {
    Charging,
    Discharging,
    Full,
    #[default]
    Unknown,
}

impl BatteryModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let device = find_battery();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
            let state = this.read_with(cx, |m, _| read_battery(m.device.as_deref()));
            let Ok((cap, status)) = state else { break };
            if this
                .update(cx, |m, cx| {
                    if m.capacity != cap || m.status != status {
                        m.capacity = cap;
                        m.status = status;
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        let (capacity, status) = read_battery(device.as_deref());
        BatteryModule {
            capacity,
            status,
            device,
        }
    }
}

fn find_battery() -> Option<String> {
    fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("BAT"))
        .map(|e| e.file_name().to_string_lossy().to_string())
}

fn read_battery(device: Option<&str>) -> (Option<u8>, BatteryStatus) {
    let Some(bat) = device else {
        return (None, BatteryStatus::Unknown);
    };
    let base = format!("/sys/class/power_supply/{bat}");
    let capacity = fs::read_to_string(format!("{base}/capacity"))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let status = fs::read_to_string(format!("{base}/status"))
        .ok()
        .map(|s| match s.trim() {
            "Charging" => BatteryStatus::Charging,
            "Full" => BatteryStatus::Full,
            "Discharging" | "Not charging" => BatteryStatus::Discharging,
            _ => BatteryStatus::Unknown,
        })
        .unwrap_or_default();
    (capacity, status)
}

impl Render for BatteryModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(cap) = self.capacity else {
            return div();
        };
        let icon = match (&self.status, cap) {
            (BatteryStatus::Charging, _) => "󰂄",
            (_, 0..=10) => "󰁺",
            (_, 11..=30) => "󰁼",
            (_, 31..=60) => "󰁾",
            (_, 61..=90) => "󰂀",
            _ => "󰁹",
        };
        let t = cx.global::<Theme>();
        let color = match cap {
            0..=10 => t.urgent,
            11..=25 => t.warning,
            _ => match &self.status {
                BatteryStatus::Charging => t.success,
                _ => t.fg_dim,
            },
        };
        theme::pill(cx)
            .flex()
            .items_center()
            .gap_1()
            .text_color(color)
            .child(icon.to_string())
            .child(theme::pct_label(cap, color))
    }
}
