use std::sync::Arc;
use gpui::{
    Context, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window,
    div, prelude::*,
};
use crate::backend::{
    WorkspaceBackend, WorkspaceEvent, WorkspaceState,
};
use crate::theme;

pub struct WorkspacesModule {
    state: WorkspaceState,
    backend: Option<Arc<dyn WorkspaceBackend>>,
}

impl WorkspacesModule {
    pub fn new(
        backend: Option<Arc<dyn WorkspaceBackend>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let Some(backend) = backend else {
            return WorkspacesModule {
                state: WorkspaceState::default(),
                backend: None,
            };
        };

        let (tx, rx) = async_channel::unbounded::<WorkspaceEvent>();

        // Start the backend's main session task.
        cx.spawn({
            let backend = backend.clone();
            async move |_this, cx| {
                let task = backend.run(tx, cx);
                task.await;
            }
        }).detach();

        // Poll the receiver on the UI executor and apply events.
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = rx.recv().await {
                if this.update(cx, |m, cx| { m.apply(ev); cx.notify(); }).is_err() {
                    return;
                }
            }
        }).detach();

        WorkspacesModule {
            state: WorkspaceState::default(),
            backend: Some(backend),
        }
    }

    fn apply(&mut self, ev: WorkspaceEvent) {
        match ev {
            WorkspaceEvent::Snapshot(s) => self.state = s,
            WorkspaceEvent::Disconnected => self.state = WorkspaceState::default(),
        }
    }
}

impl Render for WorkspacesModule {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().items_center().gap(theme::MODULE_GAP);
        for ws in &self.state.workspaces {
            let id = ws.id.clone();
            let bg = if ws.active {
                theme::accent()
            } else if ws.urgent {
                gpui::rgb(0xf38ba8).into()
            } else {
                theme::muted()
            };
            let mut pill = div()
                .px_2()
                .py_0p5()
                .rounded_md()
                .bg(bg)
                .child(ws.name.clone());
            if let Some(backend) = self.backend.as_ref() {
                let backend = backend.clone();
                pill = pill.on_mouse_down(
                    MouseButton::Left,
                    move |_e: &MouseDownEvent, _w, _cx| {
                        backend.activate(&id);
                    },
                );
            }
            row = row.child(pill);
        }
        row
    }
}
