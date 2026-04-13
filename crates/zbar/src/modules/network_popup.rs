//! Layer-shell popup that shows per-interface network details sourced from
//! netlink (see `crate::net_info`).

use std::collections::HashMap;

use gpui::{
    actions, div, prelude::*, px, App, Context, FocusHandle, Focusable, Hsla, KeyBinding,
    MouseButton, MouseMoveEvent, Window,
};

use crate::net_info::{
    format_bytes, format_mac, format_rate_long, IfaceSnapshot, NetEvent, OperState,
};
use crate::theme;

actions!(zbar_netpopup, [Dismiss]);

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", Dismiss, Some("NetworkPopup"))]
}

pub struct NetworkPopup {
    interfaces: Vec<IfaceSnapshot>,
    rates: HashMap<u32, (f64, f64)>,
    selected: Option<u32>,
    hovered: Option<u32>,
    focus_handle: FocusHandle,
}

impl NetworkPopup {
    pub fn new(rx: async_channel::Receiver<NetEvent>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        cx.spawn(async move |this, cx| {
            while let Ok(evt) = rx.recv().await {
                if this.update(cx, |m, cx| m.handle_event(evt, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            interfaces: Vec::new(),
            rates: HashMap::new(),
            selected: None,
            hovered: None,
            focus_handle,
        }
    }

    fn handle_event(&mut self, evt: NetEvent, cx: &mut Context<Self>) {
        match evt {
            NetEvent::Snapshot(list) => {
                let pick_default = |l: &[IfaceSnapshot]| -> Option<u32> {
                    l.iter()
                        .find(|i| i.is_physical && i.operstate.is_up())
                        .or_else(|| l.iter().find(|i| i.operstate.is_up()))
                        .or_else(|| l.first())
                        .map(|i| i.index)
                };
                if self.selected.is_none() {
                    self.selected = pick_default(&list);
                } else if let Some(sel) = self.selected {
                    if !list.iter().any(|i| i.index == sel) {
                        self.selected = pick_default(&list);
                    }
                }
                if self.interfaces != list {
                    self.interfaces = list;
                    cx.notify();
                }
            }
            NetEvent::Rates(r) => {
                if self.rates != r {
                    self.rates = r;
                    cx.notify();
                }
            }
        }
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    fn active_iface(&self) -> Option<&IfaceSnapshot> {
        let idx = self.hovered.or(self.selected)?;
        self.interfaces.iter().find(|i| i.index == idx)
    }
}

impl Focusable for NetworkPopup {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NetworkPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let left = self.render_iface_list(cx);
        let right = self.render_details();

        div()
            .key_context("NetworkPopup")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .bg(theme::bg())
            .text_color(theme::fg())
            .text_size(theme::FONT_SIZE)
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .p_2()
            .flex()
            .gap_2()
            .child(left)
            .child(right)
    }
}

impl NetworkPopup {
    fn render_iface_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut col = div()
            .w(px(140.))
            .flex()
            .flex_col()
            .gap_1()
            .overflow_hidden();

        let entity = cx.entity().clone();
        for iface in &self.interfaces {
            let idx = iface.index;
            let is_selected = self.selected == Some(idx);
            let is_hovered = self.hovered == Some(idx);
            let (icon, icon_color) = iface_icon(iface);
            let state_color = operstate_color(iface.operstate);

            let bg = if is_selected {
                theme::accent_dim()
            } else if is_hovered {
                theme::surface_hover()
            } else {
                Hsla::transparent_black()
            };
            let text_color = if is_selected {
                theme::fg()
            } else {
                theme::fg_dim()
            };

            let entity_click = entity.clone();
            let entity_move = entity.clone();
            let row = div()
                .id(("iface", idx as usize))
                .flex()
                .items_center()
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .bg(bg)
                .text_color(text_color)
                .cursor_pointer()
                .on_mouse_move(move |_e: &MouseMoveEvent, _w, cx| {
                    entity_move.update(cx, |m, cx| {
                        if m.hovered != Some(idx) {
                            m.hovered = Some(idx);
                            cx.notify();
                        }
                    });
                })
                .on_mouse_down(MouseButton::Left, move |_, _w, cx| {
                    entity_click.update(cx, |m, cx| {
                        m.selected = Some(idx);
                        cx.notify();
                    });
                })
                .child(div().text_color(icon_color).child(icon.to_string()))
                .child(div().flex_1().overflow_x_hidden().child(iface.name.clone()))
                .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(state_color));

            col = col.child(row);
        }

        if self.interfaces.is_empty() {
            col = col.child(
                div()
                    .text_color(theme::fg_dim())
                    .child("no interfaces".to_string()),
            );
        }

        col
    }

    fn render_details(&self) -> gpui::Div {
        let Some(iface) = self.active_iface() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::fg_dim())
                .child("select an interface".to_string());
        };

        let (icon, icon_color) = iface_icon(iface);
        let state_color = operstate_color(iface.operstate);

        let mac_text = iface.mac.map(format_mac).unwrap_or_else(|| "—".to_string());

        let (rx_bps, tx_bps) = self.rates.get(&iface.index).copied().unwrap_or((0.0, 0.0));

        let mut col = div().flex_1().flex().flex_col().gap_0p5().overflow_hidden();

        col = col.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_color(icon_color).child(icon.to_string()))
                .child(div().child(iface.name.clone()))
                .child(
                    div()
                        .text_color(state_color)
                        .child(format!("[{}]", iface.operstate.as_str())),
                ),
        );

        if let Some(ssid) = iface.wifi_ssid.as_ref() {
            col = col.child(
                div()
                    .flex()
                    .gap_1()
                    .child(div().text_color(theme::fg_dim()).child("SSID"))
                    .child(div().child(ssid.clone())),
            );
        }

        col = col.child(
            div()
                .flex()
                .gap_1()
                .child(div().text_color(theme::fg_dim()).child("MAC "))
                .child(div().child(mac_text)),
        );

        if iface.ipv4.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .gap_1()
                    .child(div().text_color(theme::fg_dim()).child("IPv4"))
                    .child(div().text_color(theme::fg_dim()).child("—".to_string())),
            );
        } else {
            for v in &iface.ipv4 {
                col = col.child(
                    div()
                        .flex()
                        .gap_1()
                        .child(div().text_color(theme::fg_dim()).child("IPv4"))
                        .child(div().overflow_x_hidden().child(v.to_string())),
                );
            }
        }

        if iface.ipv6.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .gap_1()
                    .child(div().text_color(theme::fg_dim()).child("IPv6"))
                    .child(div().text_color(theme::fg_dim()).child("—".to_string())),
            );
        } else {
            for v in &iface.ipv6 {
                col = col.child(
                    div()
                        .flex()
                        .gap_1()
                        .child(div().text_color(theme::fg_dim()).child("IPv6"))
                        .child(div().overflow_x_hidden().child(v.to_string())),
                );
            }
        }

        col = col.child(
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .text_color(theme::green())
                        .child(format!("↓ {}", format_rate_long(rx_bps))),
                )
                .child(
                    div()
                        .text_color(theme::accent())
                        .child(format!("↑ {}", format_rate_long(tx_bps))),
                ),
        );

        col = col.child(
            div()
                .flex()
                .gap_2()
                .text_color(theme::fg_dim())
                .child(div().child(format!("Rx {}", format_bytes(iface.rx_bytes))))
                .child(div().child(format!("Tx {}", format_bytes(iface.tx_bytes)))),
        );

        col
    }
}

fn iface_icon(iface: &IfaceSnapshot) -> (&'static str, Hsla) {
    let color = if iface.operstate.is_up() {
        theme::green()
    } else {
        theme::fg_dim()
    };
    if iface.is_wireless {
        ("\u{f0928}", color) // 󰤨
    } else {
        ("\u{f0200}", color) // 󰈀
    }
}

fn operstate_color(state: OperState) -> Hsla {
    match state {
        OperState::Up => theme::green(),
        OperState::Down => theme::urgent(),
        OperState::Unknown => theme::fg_dim(),
    }
}
