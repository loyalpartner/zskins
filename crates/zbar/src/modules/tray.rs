use crate::theme;
use gpui::{
    div, img, Context, ImageSource, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Render, RenderImage, Styled, Window,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct TrayModule {
    items: BTreeMap<String, TrayItem>,
    /// Channel to send activation requests back to the D-Bus thread.
    activate_tx: async_channel::Sender<ActivateReq>,
}

struct TrayItem {
    icon: Option<TrayIcon>,
    status: ItemStatus,
}

/// Icon data — either pre-decoded pixels (from IconPixmap) or an encoded
/// image file (PNG/SVG) that GPUI will decode at render time.
#[derive(Clone)]
enum TrayIcon {
    Pixmap(Arc<RenderImage>),
    File(Arc<gpui::Image>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ItemStatus {
    #[default]
    Active,
    Passive,
    NeedsAttention,
}

impl ItemStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "Passive" => Self::Passive,
            "NeedsAttention" => Self::NeedsAttention,
            _ => Self::Active,
        }
    }
}

enum TrayMsg {
    Add(String, Option<TrayIcon>, ItemStatus),
    UpdateIcon(String, Option<TrayIcon>),
    UpdateStatus(String, ItemStatus),
    Remove(String),
}

/// A request from the GPUI thread to perform an action on a tray item.
#[allow(dead_code)]
enum ActivateReq {
    Default(String),
    Secondary(String),
}

// ---------------------------------------------------------------------------
// zbus proxy definitions
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
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;

    #[zbus(signal)]
    fn new_icon(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn new_status(&self, status: &str) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Module implementation
// ---------------------------------------------------------------------------

impl TrayModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded::<TrayMsg>(32);
        let (activate_tx, activate_rx) = async_channel::bounded::<ActivateReq>(8);

        cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this
                    .update(cx, |m, cx| {
                        let changed = match msg {
                            TrayMsg::Add(addr, icon, status) => {
                                m.items.insert(addr, TrayItem { icon, status });
                                true
                            }
                            TrayMsg::UpdateIcon(addr, icon) => {
                                if let Some(item) = m.items.get_mut(&addr) {
                                    item.icon = icon;
                                    true
                                } else {
                                    false
                                }
                            }
                            TrayMsg::UpdateStatus(addr, status) => {
                                if let Some(item) = m.items.get_mut(&addr) {
                                    if item.status != status {
                                        item.status = status;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
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

        std::thread::Builder::new()
            .name("tray-sni".into())
            .spawn(move || {
                async_io::block_on(run_sni_host(tx, activate_rx));
            })
            .expect("spawn tray thread");

        TrayModule {
            items: BTreeMap::new(),
            activate_tx,
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal StatusNotifierWatcher server
// ---------------------------------------------------------------------------

const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const ITEM_OBJECT: &str = "/StatusNotifierItem";

#[allow(dead_code)] // ItemRemoved will be used when disconnect detection is added.
enum WatcherEvent {
    ItemAdded(String),
    ItemRemoved(String),
}

struct SniWatcher {
    items: std::sync::Mutex<std::collections::HashSet<String>>,
    hosts: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Notify the host directly when items are added/removed
    /// (D-Bus doesn't loop back signals to the same connection).
    event_tx: async_channel::Sender<WatcherEvent>,
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
        let _ = self
            .event_tx
            .try_send(WatcherEvent::ItemAdded(item.clone()));
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

fn resolve_sender(service: &str, hdr: &zbus::message::Header<'_>) -> String {
    if service.starts_with('/') {
        if let Some(sender) = hdr.sender() {
            return format!("{sender}{service}");
        }
    }
    service.to_string()
}

async fn start_watcher(
    conn: &zbus::Connection,
) -> anyhow::Result<async_channel::Receiver<WatcherEvent>> {
    let (event_tx, event_rx) = async_channel::bounded(32);
    let watcher = SniWatcher {
        items: Default::default(),
        hosts: Default::default(),
        event_tx,
    };
    conn.object_server().at(WATCHER_OBJECT, watcher).await?;

    use zbus::fdo::RequestNameFlags;
    let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::DoNotQueue;
    match conn.request_name_with_flags(WATCHER_BUS, flags).await {
        Ok(_) | Err(zbus::Error::NameTaken) => {}
        Err(e) => return Err(e.into()),
    }
    tracing::info!("tray: StatusNotifierWatcher started");
    Ok(event_rx)
}

// ---------------------------------------------------------------------------
// SNI host logic
// ---------------------------------------------------------------------------

async fn run_sni_host(
    tx: async_channel::Sender<TrayMsg>,
    activate_rx: async_channel::Receiver<ActivateReq>,
) {
    let mut delay_ms: u64 = 1000;
    loop {
        match run_sni_session(&tx, &activate_rx).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!("tray: SNI session failed: {e:#}; reconnecting in {delay_ms}ms");
                async_io::Timer::after(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(30_000);
            }
        }
    }
}

async fn run_sni_session(
    tx: &async_channel::Sender<TrayMsg>,
    activate_rx: &async_channel::Receiver<ActivateReq>,
) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;

    // Start our Watcher — returns a channel for item registrations
    // (D-Bus doesn't loop signals back to the same connection).
    let watcher_event_rx = match start_watcher(&conn).await {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::debug!("tray: watcher start skipped: {e}");
            None
        }
    };

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

    let icon_cache = Arc::new(icon_theme::IconCache::new(&[
        "apps", "status", "devices", "actions",
    ]));

    let ex = async_executor::LocalExecutor::new();

    match watcher.registered_status_notifier_items().await {
        Ok(items) => {
            tracing::info!("tray: found {} initial item(s)", items.len());
            for addr in items {
                add_item(&conn, &addr, &icon_cache, tx, &ex).await;
            }
        }
        Err(e) => {
            tracing::warn!("tray: failed to get initial items: {e}");
        }
    }

    loop {
        futures_lite::future::or(
            ex.tick(),
            futures_lite::future::or(
                async {
                    if let Some(ref rx) = watcher_event_rx {
                        if let Ok(event) = rx.recv().await {
                            match event {
                                WatcherEvent::ItemAdded(addr) => {
                                    tracing::debug!("tray item added: {addr}");
                                    add_item(&conn, &addr, &icon_cache, tx, &ex).await;
                                }
                                WatcherEvent::ItemRemoved(addr) => {
                                    tracing::debug!("tray item removed: {addr}");
                                    let _ = tx.send(TrayMsg::Remove(addr)).await;
                                }
                            }
                        }
                    } else {
                        futures_lite::future::pending::<()>().await;
                    }
                },
                async {
                    if let Ok(req) = activate_rx.recv().await {
                        handle_activate(&conn, req).await;
                    }
                },
            ),
        )
        .await;

        if tx.is_closed() {
            return Ok(());
        }
    }
}

/// Build a fresh proxy on-demand and activate.
/// This avoids caching proxies with unsafe lifetime transmute.
async fn handle_activate(conn: &zbus::Connection, req: ActivateReq) {
    let addr = match &req {
        ActivateReq::Default(a) | ActivateReq::Secondary(a) => a.as_str(),
    };
    let (destination, path) = parse_address(addr);
    let proxy = match StatusNotifierItemProxy::builder(conn)
        .destination(destination.to_string())
        .and_then(|b| b.path(path.to_string()))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("tray: activate proxy failed for {addr}: {e}");
                return;
            }
        },
        Err(e) => {
            tracing::warn!("tray: activate invalid address {addr}: {e}");
            return;
        }
    };
    let result = match req {
        ActivateReq::Default(_) => proxy.activate(0, 0).await,
        ActivateReq::Secondary(_) => proxy.secondary_activate(0, 0).await,
    };
    if let Err(e) = result {
        tracing::warn!("tray: activate failed for {addr}: {e}");
    }
}

async fn add_item(
    conn: &zbus::Connection,
    addr: &str,
    icon_cache: &Arc<icon_theme::IconCache>,
    tx: &async_channel::Sender<TrayMsg>,
    ex: &async_executor::LocalExecutor<'_>,
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

    let icon = fetch_icon(&proxy, icon_cache).await;
    let status = proxy
        .status()
        .await
        .map(|s| ItemStatus::from_str(&s))
        .unwrap_or_default();

    let _ = tx.send(TrayMsg::Add(addr.to_string(), icon, status)).await;

    // Spawn NewIcon watcher.
    {
        let tx = tx.clone();
        let addr = addr.to_string();
        let icon_cache = icon_cache.clone();
        let proxy_inner = proxy.inner().clone();
        ex.spawn(async move {
            use futures_lite::StreamExt;
            let proxy = StatusNotifierItemProxy::from(proxy_inner);
            let Ok(mut stream) = proxy.receive_new_icon().await else {
                tracing::debug!("tray: failed to subscribe NewIcon for {addr}");
                return;
            };
            tracing::debug!("tray: watching NewIcon for {addr}");
            while stream.next().await.is_some() {
                tracing::debug!("tray: NewIcon signal for {addr}");
                let icon = fetch_icon(&proxy, &icon_cache).await;
                if tx
                    .send(TrayMsg::UpdateIcon(addr.clone(), icon))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    // Spawn NewStatus watcher.
    {
        let tx = tx.clone();
        let addr = addr.to_string();
        let proxy_inner = proxy.inner().clone();
        ex.spawn(async move {
            use futures_lite::StreamExt;
            let proxy = StatusNotifierItemProxy::from(proxy_inner);
            let Ok(mut stream) = proxy.receive_new_status().await else {
                return;
            };
            while let Some(sig) = stream.next().await {
                if let Ok(args) = sig.args() {
                    let status = ItemStatus::from_str(args.status);
                    if tx
                        .send(TrayMsg::UpdateStatus(addr.clone(), status))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .detach();
    }
}

/// Fetch the current icon, bypassing zbus property cache by using
/// Properties.Get directly (NewIcon signal arrives before PropertiesChanged).
async fn fetch_icon(
    proxy: &StatusNotifierItemProxy<'_>,
    icon_cache: &icon_theme::IconCache,
) -> Option<TrayIcon> {
    let props = zbus::fdo::PropertiesProxy::builder(proxy.inner().connection())
        .destination(proxy.inner().destination().to_string())
        .ok()?
        .path(proxy.inner().path().to_string())
        .ok()?
        .build()
        .await
        .ok()?;

    let sni_iface =
        zbus::names::InterfaceName::from_static_str_unchecked("org.kde.StatusNotifierItem");

    // Try IconPixmap first.
    if let Ok(val) = props.get(sni_iface.clone(), "IconPixmap").await {
        if let Ok(pixmaps) = <Vec<(i32, i32, Vec<u8>)>>::try_from(val) {
            tracing::debug!("tray: IconPixmap returned {} entries", pixmaps.len());
            if let Some(img) = best_pixmap_from_tuples(&pixmaps) {
                return Some(TrayIcon::Pixmap(img));
            }
        }
    }

    // Fall back to IconName.
    let name: String = props
        .get(sni_iface.clone(), "IconName")
        .await
        .ok()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default();
    if name.is_empty() {
        tracing::debug!("tray: no icon_name available");
        return None;
    }

    if name.starts_with('/') {
        return load_icon_file(std::path::Path::new(&name));
    }

    // Check app-specific IconThemePath first.
    let theme_path: String = props
        .get(sni_iface.clone(), "IconThemePath")
        .await
        .ok()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default();
    if !theme_path.is_empty() {
        let dir = std::path::Path::new(&theme_path);
        for ext in &["svg", "svgz", "png"] {
            let candidate = dir.join(format!("{name}.{ext}"));
            if candidate.exists() {
                tracing::debug!(
                    "tray: icon_name={name} via theme_path: {}",
                    candidate.display()
                );
                return load_icon_file(&candidate);
            }
        }
        // Also check subdirectories (e.g. hicolor/48x48/status/)
        if dir.is_dir() {
            if let Some(icon) = find_icon_recursive(dir, &name, 3) {
                tracing::debug!(
                    "tray: icon_name={name} via theme_path subdir: {}",
                    icon.display()
                );
                return load_icon_file(&icon);
            }
        }
    }

    let path = icon_cache.lookup(&name)?;
    tracing::debug!(
        "tray: icon_name={name} via system theme: {}",
        path.display()
    );
    load_icon_file(path)
}

fn find_icon_recursive(dir: &std::path::Path, name: &str, depth: u8) -> Option<std::path::PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_file() {
            if path.file_stem().and_then(|s| s.to_str()) == Some(name) {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if matches!(ext, "svg" | "svgz" | "png") {
                    return Some(path);
                }
            }
        } else if depth > 0 && path.is_dir() {
            if let Some(found) = find_icon_recursive(&path, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

static NEXT_IMAGE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn load_icon_file(path: &std::path::Path) -> Option<TrayIcon> {
    let bytes = std::fs::read(path).ok()?;
    let format = match path.extension()?.to_str()? {
        "svg" | "svgz" => gpui::ImageFormat::Svg,
        "png" => gpui::ImageFormat::Png,
        _ => return None,
    };
    // Recolor symbolic SVG icons for dark panel — Breeze/KDE use
    // .ColorScheme-Text with a dark color that's invisible on dark bg.
    let bytes = if format == gpui::ImageFormat::Svg {
        recolor_svg_for_dark_panel(bytes)
    } else {
        bytes
    };

    Some(TrayIcon::File(Arc::new(gpui::Image {
        format,
        bytes,
        id: NEXT_IMAGE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    })))
}

/// Replace dark fill colors in symbolic SVGs with a light panel foreground.
fn recolor_svg_for_dark_panel(bytes: Vec<u8>) -> Vec<u8> {
    // Must match theme::fg() = rgb(0xcdd6f4). Hardcoded because theme
    // returns Hsla which can't be cheaply converted to hex at compile time.
    const LIGHT: &str = "#cdd6f4";
    // Common dark colors used by Breeze / KDE symbolic icons.
    const DARK_COLORS: &[&str] = &["#31363b", "#232629", "#4d4d4d", "#000000"];

    let mut svg = String::from_utf8_lossy(&bytes).into_owned();
    for dark in DARK_COLORS {
        svg = svg.replace(dark, LIGHT);
    }
    svg.into_bytes()
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
            // Passive items are hidden per SNI spec.
            if item.status == ItemStatus::Passive {
                continue;
            }

            let activate_tx = self.activate_tx.clone();
            let addr_click = addr.clone();

            let base = div()
                .id(gpui::ElementId::Name(addr.clone().into()))
                .cursor_pointer()
                .rounded_sm()
                .hover(|s| s.bg(theme::surface_hover()))
                .p(gpui::px(2.0))
                .on_mouse_down(MouseButton::Left, move |_, _, _cx| {
                    let _ = activate_tx.try_send(ActivateReq::Default(addr_click.clone()));
                });

            let child = if let Some(ref icon) = item.icon {
                base.child(match icon {
                    TrayIcon::Pixmap(r) => img(ImageSource::Render(r.clone())).size(ICON_SIZE),
                    TrayIcon::File(i) => img(ImageSource::Image(i.clone())).size(ICON_SIZE),
                })
            } else {
                base.size(ICON_SIZE).bg(theme::fg_dim()).rounded_full()
            };

            // NeedsAttention: subtle visual hint.
            let child = if item.status == ItemStatus::NeedsAttention {
                child.bg(theme::accent_dim())
            } else {
                child
            };

            row = row.child(child);
        }

        row
    }
}
