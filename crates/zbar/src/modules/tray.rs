use crate::theme;
use gpui::{
    div, img, Context, ImageSource, InteractiveElement, IntoElement, ParentElement, Render,
    RenderImage, Styled, Window,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct TrayModule {
    items: BTreeMap<String, TrayItem>,
}

struct TrayItem {
    icon: Option<Arc<RenderImage>>,
}

enum TrayMsg {
    Add(String, Option<Arc<RenderImage>>),
    Remove(String),
}

// ---------------------------------------------------------------------------
// zbus proxy definitions (StatusNotifierWatcher + StatusNotifierItem)
// ---------------------------------------------------------------------------

#[zbus::proxy(
    default_service = "org.kde.StatusNotifierWatcher",
    interface = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn status_notifier_item_registered(&self, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn status_notifier_item_unregistered(&self, service: &str) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.kde.StatusNotifierItem", assume_defaults = true)]
trait StatusNotifierItem {
    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;

    #[zbus(signal)]
    fn new_icon(&self) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Module implementation
// ---------------------------------------------------------------------------

impl TrayModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded::<TrayMsg>(32);

        cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this
                    .update(cx, |m, cx| {
                        let changed = match msg {
                            TrayMsg::Add(addr, icon) => {
                                m.items.insert(addr, TrayItem { icon });
                                true
                            }
                            TrayMsg::Remove(addr) => m.items.remove(&addr).is_some(),
                        };
                        if changed {
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

        // zbus with async-io uses its own reactor — we drive it with
        // async_io::block_on on a dedicated thread (no tokio needed).
        std::thread::Builder::new()
            .name("tray-sni".into())
            .spawn(move || {
                async_io::block_on(run_sni_host(tx));
            })
            .expect("spawn tray thread");

        TrayModule {
            items: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal StatusNotifierWatcher server (no tokio)
// ---------------------------------------------------------------------------

const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const ITEM_OBJECT: &str = "/StatusNotifierItem";

#[derive(Default)]
struct SniWatcher {
    items: std::sync::Mutex<std::collections::HashSet<String>>,
    hosts: std::sync::Mutex<std::collections::HashSet<String>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl SniWatcher {
    async fn register_status_notifier_host(
        &self,
        service: &str,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let name = resolve_sender(service, &hdr);
        tracing::debug!("watcher: host registered: {name}");
        self.hosts.lock().unwrap().insert(name);
        let _ = Self::status_notifier_host_registered(&emitter).await;
    }

    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let name = resolve_sender(service, &hdr);
        let item = if name.contains('/') {
            name.clone()
        } else {
            format!("{name}{ITEM_OBJECT}")
        };
        tracing::info!("watcher: item registered: {item}");
        self.items.lock().unwrap().insert(item.clone());
        let _ = Self::status_notifier_item_registered(&emitter, &item).await;
    }

    #[zbus(signal)]
    async fn status_notifier_host_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        !self.hosts.lock().unwrap().is_empty()
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().iter().cloned().collect()
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

/// Resolve a D-Bus service string to a usable bus address.
/// Object-path-only senders (e.g. "/StatusNotifierItem") get the message
/// sender prepended; everything else is returned as-is.
fn resolve_sender(service: &str, hdr: &zbus::message::Header<'_>) -> String {
    if service.starts_with('/') {
        if let Some(sender) = hdr.sender() {
            return format!("{sender}{service}");
        }
    }
    service.to_string()
}

/// Start the Watcher on the given connection (best-effort, defers to existing).
async fn start_watcher(conn: &zbus::Connection) -> anyhow::Result<()> {
    conn.object_server()
        .at(WATCHER_OBJECT, SniWatcher::default())
        .await?;

    use zbus::fdo::RequestNameFlags;
    let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::DoNotQueue;
    match conn.request_name_with_flags(WATCHER_BUS, flags).await {
        Ok(_) | Err(zbus::Error::NameTaken) => {}
        Err(e) => return Err(e.into()),
    }
    tracing::info!("tray: StatusNotifierWatcher started");
    Ok(())
}

// ---------------------------------------------------------------------------
// SNI host logic (runs on background thread with async-io reactor)
// ---------------------------------------------------------------------------

async fn run_sni_host(tx: async_channel::Sender<TrayMsg>) {
    let mut delay_ms: u64 = 1000;
    loop {
        match run_sni_session(&tx).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!("tray: SNI session failed: {e:#}; reconnecting in {delay_ms}ms");
                async_io::Timer::after(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(30_000);
            }
        }
    }
}

async fn run_sni_session(tx: &async_channel::Sender<TrayMsg>) -> anyhow::Result<()> {
    use futures_lite::StreamExt;

    let conn = zbus::Connection::session().await?;

    if let Err(e) = start_watcher(&conn).await {
        tracing::debug!("tray: watcher start skipped: {e}");
    }

    let watcher = StatusNotifierWatcherProxy::new(&conn).await?;

    let pid = std::process::id();
    let host_name = format!("org.freedesktop.StatusNotifierHost-{pid}");
    let host_wellknown: zbus::names::WellKnownName = host_name
        .as_str()
        .try_into()
        .map_err(|e| anyhow::anyhow!("invalid host name: {e}"))?;
    conn.request_name(&host_wellknown).await?;
    watcher.register_status_notifier_host(&host_name).await?;
    tracing::info!("tray: registered as SNI host");

    match watcher.registered_status_notifier_items().await {
        Ok(items) => {
            tracing::info!("tray: found {} initial item(s)", items.len());
            for addr in items {
                fetch_and_send_item(&conn, &addr, tx).await;
            }
        }
        Err(e) => {
            tracing::warn!("tray: failed to get initial items: {e}");
        }
    }

    let mut added = watcher.receive_status_notifier_item_registered().await?;
    let mut removed = watcher.receive_status_notifier_item_unregistered().await?;

    // Process both streams concurrently; `or` returns when either yields,
    // but both streams are polled each iteration so no signals are lost.
    loop {
        futures_lite::future::or(
            async {
                if let Some(sig) = added.next().await {
                    if let Ok(args) = sig.args() {
                        let addr = args.service.to_string();
                        tracing::debug!("tray item added: {addr}");
                        fetch_and_send_item(&conn, &addr, tx).await;
                    }
                }
            },
            async {
                if let Some(sig) = removed.next().await {
                    if let Ok(args) = sig.args() {
                        let addr = args.service.to_string();
                        tracing::debug!("tray item removed: {addr}");
                        let _ = tx.send(TrayMsg::Remove(addr)).await;
                    }
                }
            },
        )
        .await;

        if tx.is_closed() {
            return Ok(());
        }
    }
}

async fn fetch_and_send_item(
    conn: &zbus::Connection,
    addr: &str,
    tx: &async_channel::Sender<TrayMsg>,
) {
    let (destination, path) = parse_address(addr);
    let proxy = match StatusNotifierItemProxy::builder(conn)
        .destination(destination.to_string())
        .and_then(|b| b.path(path.to_string()))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("tray: failed to build proxy for {addr}: {e}");
                return;
            }
        },
        Err(e) => {
            tracing::warn!("tray: invalid address {addr}: {e}");
            return;
        }
    };

    let icon = proxy
        .icon_pixmap()
        .await
        .ok()
        .and_then(|pixmaps| best_pixmap_from_tuples(&pixmaps));

    let _ = tx.send(TrayMsg::Add(addr.to_string(), icon)).await;
}

fn parse_address(address: &str) -> (&str, &str) {
    if let Some(slash) = address.find('/') {
        (&address[..slash], &address[slash..])
    } else {
        (address, "/StatusNotifierItem")
    }
}

// ---------------------------------------------------------------------------
// Icon conversion
// ---------------------------------------------------------------------------

fn best_pixmap_from_tuples(pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<Arc<RenderImage>> {
    pixmaps
        .iter()
        .max_by_key(|(w, h, _)| w * h)
        .and_then(|(w, h, pixels)| render_argb_pixmap(*w as u32, *h as u32, pixels))
}

/// Convert ARGB32 (network byte order) pixel data to a GPUI RenderImage.
fn render_argb_pixmap(w: u32, h: u32, pixels: &[u8]) -> Option<Arc<RenderImage>> {
    if pixels.len() != (w * h * 4) as usize {
        tracing::warn!(
            "tray icon pixmap size mismatch: {w}x{h} but {} bytes",
            pixels.len()
        );
        return None;
    }

    // SNI spec: ARGB32 network byte order → [A, R, G, B] per pixel.
    // image::RgbaImage expects [R, G, B, A].
    let mut rgba = Vec::with_capacity(pixels.len());
    for chunk in pixels.chunks_exact(4) {
        rgba.extend_from_slice(&[chunk[1], chunk[2], chunk[3], chunk[0]]);
    }

    let buf = image::RgbaImage::from_raw(w, h, rgba)?;
    let frame = image::Frame::new(buf);
    Some(Arc::new(RenderImage::new(vec![frame])))
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

const ICON_SIZE: gpui::Pixels = gpui::px(18.0);

impl Render for TrayModule {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().items_center().gap_0p5();

        for (addr, item) in &self.items {
            let base = div()
                .id(gpui::ElementId::Name(addr.clone().into()))
                .cursor_pointer()
                .rounded_sm()
                .hover(|s| s.bg(theme::surface_hover()))
                .p(gpui::px(2.0));

            let child = if let Some(ref icon) = item.icon {
                base.child(img(ImageSource::Render(icon.clone())).size(ICON_SIZE))
            } else {
                base.size(ICON_SIZE).bg(theme::fg_dim()).rounded_full()
            };
            row = row.child(child);
        }

        row
    }
}
