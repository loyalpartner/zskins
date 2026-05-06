use crate::theme;
use gpui::{div, Context, IntoElement, ParentElement, Render, Styled, Window};
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use ztheme::Theme;

#[derive(Debug, thiserror::Error)]
enum VolumeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("pactl subscribe exited")]
    PactlExited,
}

pub struct VolumeModule {
    percent: Option<u32>,
    muted: bool,
}

impl VolumeModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (percent, muted) = read_volume();
        let (tx, rx) = async_channel::bounded::<(Option<u32>, bool)>(4);

        cx.spawn(async move |this, cx| {
            while let Ok((percent, muted)) = rx.recv().await {
                if this
                    .update(cx, |m, cx| {
                        if m.percent != percent || m.muted != muted {
                            m.percent = percent;
                            m.muted = muted;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        std::thread::Builder::new()
            .name("volume-pactl-subscribe".into())
            .spawn(move || run_pactl_subscribe(tx))
            .expect("spawn volume thread");

        VolumeModule { percent, muted }
    }
}

fn run_pactl_subscribe(tx: async_channel::Sender<(Option<u32>, bool)>) {
    let mut delay_ms: u64 = 1000;
    loop {
        let result = run_pactl_session(&tx);
        match result {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!("pactl subscribe failed: {e:#}; reconnecting in {delay_ms}ms");
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(30_000);
            }
        }
    }
}

fn run_pactl_session(tx: &async_channel::Sender<(Option<u32>, bool)>) -> Result<(), VolumeError> {
    let mut child = Command::new("pactl")
        .arg("subscribe")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::io::BufReader::new(stdout);

    for line in reader.lines() {
        let line = line?;
        if line.contains(" on sink #") {
            if tx.is_closed() {
                let _ = child.kill();
                return Ok(());
            }
            let _ = tx.try_send(read_volume());
        }
    }
    Err(VolumeError::PactlExited)
}

static HAS_WPCTL: OnceLock<bool> = OnceLock::new();

fn read_volume() -> (Option<u32>, bool) {
    let use_wpctl = *HAS_WPCTL.get_or_init(|| {
        Command::new("wpctl")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    });
    if use_wpctl {
        if let Some(result) = read_wpctl() {
            return result;
        }
    }
    read_pactl().unwrap_or((None, false))
}

fn read_wpctl() -> Option<(Option<u32>, bool)> {
    let output = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let muted = text.contains("[MUTED]");
    let vol = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<f32>().ok())
        .map(|v| (v * 100.0).round() as u32);
    Some((vol, muted))
}

fn read_pactl() -> Option<(Option<u32>, bool)> {
    let output = Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let vol = text
        .split('/')
        .nth(1)
        .and_then(|s| s.trim().trim_end_matches('%').parse().ok());

    let mute_output = Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    let muted = String::from_utf8_lossy(&mute_output.stdout).contains("yes");

    Some((vol, muted))
}

impl Render for VolumeModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(vol) = self.percent else {
            return div();
        };
        let t = *cx.global::<Theme>();
        let (icon, icon_color) = if self.muted {
            ("󰝟", t.urgent)
        } else {
            match vol {
                0 => ("󰕿", t.fg_dim),
                1..=50 => ("󰖀", t.accent),
                _ => ("󰕾", t.accent),
            }
        };
        let text_color = if self.muted { t.urgent } else { t.fg_dim };
        theme::pill(cx)
            .bg(gpui::Hsla::transparent_black())
            .flex()
            .items_center()
            .gap_0p5()
            .child(div().text_color(icon_color).child(icon.to_string()))
            .child(theme::pct_label(vol, text_color))
    }
}
