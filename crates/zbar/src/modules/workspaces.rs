use crate::backend::{WorkspaceBackend, WorkspaceEvent, WorkspaceId, WorkspaceState};
use crate::theme;
use gpui::{
    div, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Render, Styled, Window,
};
use std::sync::Arc;

pub struct WorkspacesModule {
    state: WorkspaceState,
    backend: Option<Arc<dyn WorkspaceBackend>>,
    /// Output name this module renders for (e.g. "DP-1"). `None` means the
    /// module doesn't know its output and renders all workspaces.
    output_name: Option<String>,
}

impl WorkspacesModule {
    pub fn new(
        backend: Option<Arc<dyn WorkspaceBackend>>,
        output_name: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let Some(backend) = backend else {
            return WorkspacesModule {
                state: WorkspaceState::default(),
                backend: None,
                output_name,
            };
        };

        let (tx, rx) = async_channel::bounded::<WorkspaceEvent>(64);

        cx.spawn({
            let backend = backend.clone();
            async move |this, cx| {
                let _task = backend.run(tx, cx);
                while let Ok(ev) = rx.recv().await {
                    if this
                        .update(cx, |m, cx| {
                            m.apply(ev);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .detach();

        WorkspacesModule {
            state: WorkspaceState::default(),
            backend: Some(backend),
            output_name,
        }
    }

    fn apply(&mut self, ev: WorkspaceEvent) {
        match ev {
            WorkspaceEvent::Snapshot(s) => self.state = s,
            WorkspaceEvent::Focus(id) => {
                for ws in &mut self.state.workspaces {
                    ws.active = ws.id == id;
                }
                self.state.active = Some(id);
            }
            WorkspaceEvent::Disconnected => self.state = WorkspaceState::default(),
        }
    }

    fn activate_optimistic(&mut self, target: &WorkspaceId) {
        for ws in &mut self.state.workspaces {
            if self.output_name.is_none() || ws.output.as_deref() == self.output_name.as_deref() {
                ws.active = ws.id == *target;
            }
        }
        self.state.active = Some(target.clone());
    }
}

impl Render for WorkspacesModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().items_center().gap_1();
        let my_output = self.output_name.as_deref();
        for ws in &self.state.workspaces {
            // Filter to workspaces on this bar's output when known; fall back
            // to showing every workspace if either side is unknown.
            if let (Some(my), Some(ws_out)) = (my_output, ws.output.as_deref()) {
                if my != ws_out {
                    continue;
                }
            }
            let id = ws.id.clone();
            let output_for_click = ws.output.clone();
            let (bg, hover_bg, text_color, font_weight) = if ws.active {
                (
                    theme::accent_dim(),
                    theme::accent_dim_hover(),
                    theme::accent(),
                    gpui::FontWeight::SEMIBOLD,
                )
            } else if ws.urgent {
                (
                    theme::surface(),
                    theme::surface_hover(),
                    theme::urgent(),
                    gpui::FontWeight::MEDIUM,
                )
            } else {
                (
                    gpui::Hsla::transparent_black(),
                    theme::surface_hover(),
                    theme::fg_dim(),
                    gpui::FontWeight::NORMAL,
                )
            };
            let mut pill = div()
                .px(theme::PILL_PX)
                .py(theme::PILL_PY)
                .rounded_md()
                .bg(bg)
                .text_color(text_color)
                .font_weight(font_weight)
                .hover(move |s| s.bg(hover_bg))
                .cursor_pointer()
                .child(ws.name.clone());
            if let Some(backend) = self.backend.as_ref() {
                let backend = backend.clone();
                let entity = cx.entity().clone();
                pill = pill.on_mouse_down(MouseButton::Left, move |_e: &MouseDownEvent, _w, cx| {
                    tracing::debug!(
                        "workspace click: name={} output={:?}",
                        id.0,
                        output_for_click
                    );
                    entity.update(cx, |m, cx| {
                        m.activate_optimistic(&id);
                        cx.notify();
                    });
                    backend.activate(&id, output_for_click.as_deref());
                });
            }
            row = row.child(pill);
        }
        row
    }
}
