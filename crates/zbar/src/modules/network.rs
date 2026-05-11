use crate::modules::network_popup::NetworkPopup;
use crate::net_info::{self, format_rate_short, IfaceSnapshot, NetEvent};
use crate::theme;
use gpui::{
    div, layer_shell::*, point, px, AppContext, Bounds, Context, DisplayId, InteractiveElement,
    IntoElement, ParentElement, Render, Size, StatefulInteractiveElement, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
};
use std::collections::HashMap;
use ztheme::Theme;

pub struct NetworkModule {
    interfaces: Vec<IfaceSnapshot>,
    rates: HashMap<u32, (f64, f64)>,
    display_id: Option<DisplayId>,
    open: Option<OpenPopup>,
    close_tx: async_channel::Sender<()>,
    popup_kind: crate::popup_catcher::PopupKind,
}

struct OpenPopup {
    popup: WindowHandle<NetworkPopup>,
    catchers: Vec<WindowHandle<crate::popup_catcher::PopupCatcher>>,
}

impl NetworkModule {
    pub fn new(display_id: Option<DisplayId>, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded::<NetEvent>(8);
        net_info::spawn_netlink_worker(tx);

        cx.observe_global::<Theme>(|_, cx| cx.notify()).detach();

        cx.spawn(async move |this, cx| {
            while let Ok(evt) = rx.recv().await {
                if this
                    .update(cx, |m, cx| match evt {
                        NetEvent::Snapshot(v) => {
                            if m.interfaces != v {
                                m.interfaces = v;
                                cx.notify();
                            }
                        }
                        NetEvent::Rates(r) => {
                            if m.rates != r {
                                m.rates = r;
                                cx.notify();
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let (popup_kind, close_tx) =
            crate::popup_catcher::register_entity(cx, |m, cx| m.close_popup(cx));

        NetworkModule {
            interfaces: Vec::new(),
            rates: HashMap::new(),
            display_id,
            open: None,
            close_tx,
            popup_kind,
        }
    }

    fn open_popup(&mut self, cx: &mut Context<Self>) {
        if self.open.is_some() {
            return;
        }
        crate::popup_catcher::dismiss_others(cx, self.popup_kind);
        let (tx, rx) = async_channel::bounded::<NetEvent>(8);
        net_info::spawn_netlink_worker(tx);

        let opts = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(px(420.), px(260.)),
            })),
            display_id: self.display_id,
            app_id: Some("zbar-netinfo".to_string()),
            window_background: WindowBackgroundAppearance::Transparent,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "zbar-netinfo".to_string(),
                // Overlay so the popup body sits above the catchers (Top).
                layer: Layer::Overlay,
                anchor: Anchor::TOP | Anchor::RIGHT,
                margin: Some((px(0.), px(8.), px(0.), px(0.))),
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
                exclusive_zone: None,
                ..Default::default()
            }),
            ..Default::default()
        };

        let catchers = crate::popup_catcher::open_catchers_for(
            cx,
            "zbar-netinfo-catcher",
            self.close_tx.clone(),
        );

        match cx.open_window(opts, |_, cx| cx.new(|cx| NetworkPopup::new(rx, cx))) {
            Ok(popup) => {
                self.open = Some(OpenPopup { popup, catchers });
                cx.notify();
            }
            Err(e) => {
                tracing::warn!("failed to open network popup: {e}");
                crate::popup_catcher::close_catchers(cx, catchers);
            }
        }
    }

    fn close_popup(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.open.take() {
            let _ = open.popup.update(cx, |_, window, _| window.remove_window());
            crate::popup_catcher::close_catchers(cx, open.catchers);
            cx.notify();
        }
    }
}

fn physical_label(iface: &IfaceSnapshot) -> &str {
    if iface.is_wireless {
        iface
            .wifi_ssid
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(iface.name.as_str())
    } else {
        iface.name.as_str()
    }
}

impl Render for NetworkModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().clone();
        let t = *cx.global::<Theme>();

        let physicals: Vec<&IfaceSnapshot> =
            self.interfaces.iter().filter(|i| i.is_physical).collect();

        let mut row = div()
            .id("zbar-network-row")
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .on_hover(move |hovered, _w, cx| {
                let hovered = *hovered;
                entity.update(cx, |m, cx| {
                    if hovered {
                        m.open_popup(cx);
                    } else {
                        m.close_popup(cx);
                    }
                });
            });

        if physicals.is_empty() {
            row = row.child(
                theme::pill(cx)
                    .bg(gpui::Hsla::transparent_black())
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .child(div().text_color(t.urgent).child("󰤭"))
                    .child(div().text_color(t.urgent).child("Off")),
            );
            return row;
        }

        for iface in physicals {
            let idx = iface.index;
            let up = iface.operstate.is_up();
            let icon = if iface.is_wireless { "󰤨" } else { "󰈀" };
            let icon_color = if up { t.success } else { t.urgent };
            let text_color = if up { t.fg_dim } else { t.urgent };
            let label = physical_label(iface).to_string();

            let mut pill = theme::pill(cx)
                .bg(gpui::Hsla::transparent_black())
                .flex()
                .items_center()
                .gap_0p5()
                .child(div().text_color(icon_color).child(icon.to_string()))
                .child(div().text_color(text_color).child(label));

            if up {
                let (rx, tx) = self.rates.get(&idx).copied().unwrap_or((0.0, 0.0));
                let rate = format!("↓{} ↑{}", format_rate_short(rx), format_rate_short(tx));
                pill = pill.child(
                    div()
                        .w(px(78.))
                        .flex()
                        .justify_end()
                        .overflow_x_hidden()
                        .text_color(t.fg_dim)
                        .child(rate),
                );
            }

            row = row.child(pill);
        }
        row
    }
}
