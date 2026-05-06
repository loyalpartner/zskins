use crate::theme;
use gpui::{div, Context, IntoElement, ParentElement, Render, Styled, Window};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Duration;
use ztheme::Theme;

pub struct CpuMemModule {
    cpu_percent: f32,
    mem_percent: f32,
    prev_idle: u64,
    prev_total: u64,
}

impl CpuMemModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (idle, total) = read_cpu_times();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let (idle, total) = read_cpu_times();
            let mem = read_mem_percent();
            if this
                .update(cx, |m, cx| {
                    let d_total = total.saturating_sub(m.prev_total);
                    let d_idle = idle.saturating_sub(m.prev_idle);
                    let cpu = if d_total > 0 {
                        (1.0 - d_idle as f32 / d_total as f32) * 100.0
                    } else {
                        0.0
                    };
                    m.prev_idle = idle;
                    m.prev_total = total;
                    let changed =
                        (m.cpu_percent - cpu).abs() >= 0.5 || (m.mem_percent - mem).abs() >= 0.5;
                    m.cpu_percent = cpu;
                    m.mem_percent = mem;
                    if changed {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        CpuMemModule {
            cpu_percent: 0.0,
            mem_percent: read_mem_percent(),
            prev_idle: idle,
            prev_total: total,
        }
    }
}

fn read_cpu_times() -> (u64, u64) {
    let Ok(file) = File::open("/proc/stat") else {
        return (0, 0);
    };
    let mut line = String::new();
    if BufReader::new(file).read_line(&mut line).is_err() {
        return (0, 0);
    }
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    if vals.len() < 4 {
        return (0, 0);
    }
    (vals[3], vals.iter().sum())
}

fn read_mem_percent() -> f32 {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return 0.0;
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("MemTotal:") {
            total = parse_kb(val);
        } else if let Some(val) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(val);
        }
        if total > 0 && available > 0 {
            break;
        }
    }
    if total == 0 {
        return 0.0;
    }
    ((total - available) as f32 / total as f32) * 100.0
}

fn parse_kb(s: &str) -> u64 {
    s.trim().trim_end_matches("kB").trim().parse().unwrap_or(0)
}

impl Render for CpuMemModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.global::<Theme>();
        theme::pill(cx)
            .flex()
            .items_center()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .child(div().text_color(t.accent).child(""))
                    .child(theme::pct_label(
                        format_args!("{:.0}", self.cpu_percent),
                        theme::threshold_color(cx, self.cpu_percent, 50.0, 80.0),
                    )),
            )
            .child(div().h_3().w_px().bg(t.separator))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .child(div().text_color(t.accent).child(""))
                    .child(theme::pct_label(
                        format_args!("{:.0}", self.mem_percent),
                        theme::threshold_color(cx, self.mem_percent, 65.0, 85.0),
                    )),
            )
    }
}
