use crate::theme;
use gpui::{
    div, img, Context, ImageSource, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Render, RenderImage, Styled, Window,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct TrayModule {
    items: BTreeMap<String, TrayItem>,
    activate_tx: async_channel::Sender<ActivateReq>,
    menu_click_tx: async_channel::Sender<tray_menu::MenuClickReq>,
    /// For sending CloseMenu from popup back to ourselves.
    self_tx: async_channel::Sender<TrayMsg>,
    display_id: Option<gpui::DisplayId>,
    menu_popup: Option<gpui::WindowHandle<tray_menu::TrayMenuPopup>>,
    menu_open_addr: Option<String>,
}

struct TrayItem {
    icon: Option<TrayIcon>,
    status: ItemStatus,
}

/// Icon data — either pre-decoded pixels (from IconPixmap) or an encoded
/// image file (PNG/SVG) that GPUI will decode at render time.
#[derive(Clone)]
pub(crate) enum TrayIcon {
    Pixmap(Arc<RenderImage>),
    File(Arc<gpui::Image>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ItemStatus {
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

pub(crate) enum TrayMsg {
    Add {
        addr: String,
        icon: Option<TrayIcon>,
        status: ItemStatus,
    },
    CloseMenu,
    UpdateIcon(String, Option<TrayIcon>),
    UpdateStatus(String, ItemStatus),
    Remove(String),
    /// Menu layout fetched from D-Bus, ready to display.
    MenuReady {
        addr: String,
        menu_path: String,
        items: Vec<tray_menu::MenuItem>,
        click_x: f32,
    },
}

#[allow(dead_code)]
enum ActivateReq {
    Default(String),
    Secondary(String),
    /// Right-click: fetch and show context menu. Carries click X for positioning.
    Menu(String, f32),
    /// Scroll: forward wheel delta to app (e.g. pavucontrol volume,
    /// Telegram chat switching). Orientation is "vertical" or "horizontal".
    Scroll(String, i32, &'static str),
}

use super::tray_menu;

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
    fn close_menu(&mut self, cx: &mut gpui::App) {
        if let Some(handle) = self.menu_popup.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        self.menu_open_addr = None;
    }

    pub fn new(display_id: Option<gpui::DisplayId>, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded::<TrayMsg>(32);
        let self_tx = tx.clone();
        let (activate_tx, activate_rx) = async_channel::bounded::<ActivateReq>(8);
        let (menu_click_tx, menu_click_rx) = async_channel::bounded::<tray_menu::MenuClickReq>(8);

        cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this
                    .update(cx, |m, cx| match msg {
                        TrayMsg::Add { addr, icon, status } => {
                            m.items.insert(addr, TrayItem { icon, status });
                            cx.notify();
                        }
                        TrayMsg::UpdateIcon(addr, icon) => {
                            if let Some(item) = m.items.get_mut(&addr) {
                                item.icon = icon;
                                cx.notify();
                            }
                        }
                        TrayMsg::UpdateStatus(addr, status) => {
                            if let Some(item) = m.items.get_mut(&addr) {
                                if item.status != status {
                                    item.status = status;
                                    cx.notify();
                                }
                            }
                        }
                        TrayMsg::Remove(addr) => {
                            if m.items.remove(&addr).is_some() {
                                cx.notify();
                            }
                        }
                        TrayMsg::CloseMenu => {
                            m.close_menu(cx);
                        }
                        TrayMsg::MenuReady {
                            addr,
                            menu_path,
                            items,
                            click_x,
                        } => {
                            // Toggle: if same addr menu is already open, just close.
                            if m.menu_open_addr.as_deref() == Some(&addr) {
                                m.close_menu(cx);
                                return;
                            }
                            m.close_menu(cx);
                            m.menu_popup = tray_menu::open_menu_popup(
                                cx,
                                items,
                                addr.clone(),
                                menu_path,
                                m.menu_click_tx.clone(),
                                m.self_tx.clone(),
                                m.display_id,
                                click_x,
                            );
                            m.menu_open_addr = Some(addr);
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
                async_io::block_on(run_sni_host(tx, activate_rx, menu_click_rx));
            })
            .expect("spawn tray thread");

        TrayModule {
            items: BTreeMap::new(),
            activate_tx,
            menu_click_tx,
            self_tx,
            menu_open_addr: None,
            display_id,
            menu_popup: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal StatusNotifierWatcher server
// ---------------------------------------------------------------------------

const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const ITEM_OBJECT: &str = "/StatusNotifierItem";

enum WatcherEvent {
    ItemAdded(String),
    ItemRemoved(String),
}

/// Shared mutable state between the D-Bus interface handler and the
/// NameOwnerChanged cleanup task. Maps each registered service name
/// to the unique bus name (e.g. `:1.123`) of its owner so we can
/// detect owner-loss when a tray app crashes without unregistering.
#[derive(Default)]
struct WatcherState {
    items: std::sync::Mutex<std::collections::HashMap<String, String>>,
    hosts: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

struct SniWatcher {
    state: Arc<WatcherState>,
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
        let owner = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        tracing::debug!("watcher: host registered: {name} owner={owner}");
        self.state.hosts.lock().unwrap().insert(name, owner);
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
        let owner = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        tracing::info!("watcher: item registered: {item} owner={owner}");
        self.state.items.lock().unwrap().insert(item.clone(), owner);
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
        !self.state.hosts.lock().unwrap().is_empty()
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.state.items.lock().unwrap().keys().cloned().collect()
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

struct WatcherHandle {
    event_rx: async_channel::Receiver<WatcherEvent>,
    state: Arc<WatcherState>,
    event_tx: async_channel::Sender<WatcherEvent>,
}

async fn start_watcher(conn: &zbus::Connection) -> anyhow::Result<WatcherHandle> {
    let (event_tx, event_rx) = async_channel::bounded(32);
    let state: Arc<WatcherState> = Arc::new(WatcherState::default());
    let watcher = SniWatcher {
        state: state.clone(),
        event_tx: event_tx.clone(),
    };
    conn.object_server().at(WATCHER_OBJECT, watcher).await?;

    use zbus::fdo::RequestNameFlags;
    let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::DoNotQueue;
    match conn.request_name_with_flags(WATCHER_BUS, flags).await {
        Ok(_) | Err(zbus::Error::NameTaken) => {}
        Err(e) => return Err(e.into()),
    }
    tracing::info!("tray: StatusNotifierWatcher started");
    Ok(WatcherHandle {
        event_rx,
        state,
        event_tx,
    })
}

/// Subscribe to `org.freedesktop.DBus.NameOwnerChanged` and clean up any
/// registered items/hosts whose owner disappears without an explicit
/// Unregister call (i.e. the tray app crashed or was killed).
async fn watch_name_owner_changed(
    conn: zbus::Connection,
    state: Arc<WatcherState>,
    event_tx: async_channel::Sender<WatcherEvent>,
) -> anyhow::Result<()> {
    use futures_lite::StreamExt;

    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut stream = dbus.receive_name_owner_changed().await?;
    tracing::debug!("watcher: NameOwnerChanged subscription active");

    while let Some(sig) = stream.next().await {
        let args = match sig.args() {
            Ok(a) => a,
            Err(_) => continue,
        };
        // Only fire on owner-lost transitions (new_owner empty).
        let has_new_owner = args
            .new_owner
            .as_ref()
            .is_some_and(|n| !n.as_str().is_empty());
        if has_new_owner {
            continue;
        }
        let lost = match args.old_owner.as_ref() {
            Some(n) if !n.as_str().is_empty() => n.to_string(),
            _ => continue,
        };

        // Remove any items whose owner matches the lost unique name.
        let removed_items: Vec<String> = {
            let mut items = state.items.lock().unwrap();
            let keys: Vec<String> = items
                .iter()
                .filter(|(_, v)| *v == &lost)
                .map(|(k, _)| k.clone())
                .collect();
            for k in &keys {
                items.remove(k);
            }
            keys
        };
        for item in removed_items {
            tracing::info!("watcher: item owner lost, removing: {item}");
            let _ = event_tx.try_send(WatcherEvent::ItemRemoved(item.clone()));
            // Emit the spec-defined signal so any other hosts observe the
            // cleanup too.
            if let Ok(iface_ref) = conn
                .object_server()
                .interface::<_, SniWatcher>(WATCHER_OBJECT)
                .await
            {
                let _ = SniWatcher::status_notifier_item_unregistered(
                    iface_ref.signal_emitter(),
                    &item,
                )
                .await;
            }
        }

        // Hosts cleanup — simply drop entries owned by the lost name.
        {
            let mut hosts = state.hosts.lock().unwrap();
            let before = hosts.len();
            hosts.retain(|_, owner| owner != &lost);
            let dropped = before - hosts.len();
            if dropped > 0 {
                tracing::debug!("watcher: dropped {dropped} host(s) for lost owner {lost}");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SNI host logic
// ---------------------------------------------------------------------------

async fn run_sni_host(
    tx: async_channel::Sender<TrayMsg>,
    activate_rx: async_channel::Receiver<ActivateReq>,
    menu_click_rx: async_channel::Receiver<tray_menu::MenuClickReq>,
) {
    let mut delay_ms: u64 = 1000;
    loop {
        match run_sni_session(&tx, &activate_rx, &menu_click_rx).await {
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
    menu_click_rx: &async_channel::Receiver<tray_menu::MenuClickReq>,
) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;

    // Start our Watcher — returns a channel for item registrations
    // (D-Bus doesn't loop signals back to the same connection).
    let watcher_event_rx = match start_watcher(&conn).await {
        Ok(handle) => {
            // Spawn the owner-lost cleanup task. Runs for the lifetime
            // of this session (ex.tick() below drives it).
            let cleanup_conn = conn.clone();
            let cleanup_state = handle.state.clone();
            let cleanup_tx = handle.event_tx.clone();
            std::thread::Builder::new()
                .name("tray-sni-cleanup".into())
                .spawn(move || {
                    async_io::block_on(async move {
                        if let Err(e) =
                            watch_name_owner_changed(cleanup_conn, cleanup_state, cleanup_tx).await
                        {
                            tracing::warn!("tray: NameOwnerChanged watcher ended: {e}");
                        }
                    });
                })
                .expect("spawn tray cleanup thread");
            Some(handle.event_rx)
        }
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
    let item_metas = std::cell::RefCell::new(BTreeMap::<String, TrayItemMeta>::new());

    match watcher.registered_status_notifier_items().await {
        Ok(items) => {
            tracing::info!("tray: found {} initial item(s)", items.len());
            for addr in items {
                let meta = add_item(&conn, &addr, &icon_cache, tx, &ex).await;
                item_metas.borrow_mut().insert(addr, meta);
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
                futures_lite::future::or(
                    async {
                        if let Some(ref rx) = watcher_event_rx {
                            if let Ok(event) = rx.recv().await {
                                match event {
                                    WatcherEvent::ItemAdded(addr) => {
                                        if !item_metas.borrow().contains_key(&addr) {
                                            tracing::debug!("tray item added: {addr}");
                                            let meta =
                                                add_item(&conn, &addr, &icon_cache, tx, &ex).await;
                                            item_metas.borrow_mut().insert(addr, meta);
                                        }
                                    }
                                    WatcherEvent::ItemRemoved(addr) => {
                                        tracing::debug!("tray item removed: {addr}");
                                        item_metas.borrow_mut().remove(&addr);
                                        let _ = tx.send(TrayMsg::Remove(addr)).await;
                                    }
                                }
                            }
                        } else {
                            futures_lite::future::pending::<()>().await;
                        }
                    },
                    // Menu item click from popup.
                    async {
                        if let Ok(req) = menu_click_rx.recv().await {
                            if let Err(e) = tray_menu::activate_menu_item(
                                &conn,
                                &req.addr,
                                &req.menu_path,
                                req.item_id,
                            )
                            .await
                            {
                                tracing::warn!("tray: menu click failed: {e}");
                            }
                        }
                    },
                ),
                async {
                    if let Ok(req) = activate_rx.recv().await {
                        // Extract what we need before the await to avoid
                        // holding the RefCell borrow across it.
                        let addr = match &req {
                            ActivateReq::Default(a)
                            | ActivateReq::Secondary(a)
                            | ActivateReq::Menu(a, _)
                            | ActivateReq::Scroll(a, _, _) => a.as_str(),
                        };
                        let meta = item_metas.borrow().get(addr).cloned();
                        handle_activate(&conn, tx, meta.as_ref(), req).await;
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

async fn handle_activate(
    conn: &zbus::Connection,
    tx: &async_channel::Sender<TrayMsg>,
    meta: Option<&TrayItemMeta>,
    req: ActivateReq,
) {
    let addr = match &req {
        ActivateReq::Default(a)
        | ActivateReq::Secondary(a)
        | ActivateReq::Menu(a, _)
        | ActivateReq::Scroll(a, _, _) => a.as_str(),
    };

    match req {
        ActivateReq::Menu(_, click_x) => {
            if let Some(menu_path) = meta.and_then(|m| m.menu_path.as_ref()) {
                match tray_menu::fetch_menu(conn, addr, menu_path).await {
                    Ok(menu_items) => {
                        let _ = tx
                            .send(TrayMsg::MenuReady {
                                addr: addr.to_string(),
                                menu_path: menu_path.clone(),
                                items: menu_items,
                                click_x,
                            })
                            .await;
                    }
                    Err(e) => tracing::warn!("tray: fetch menu failed for {addr}: {e}"),
                }
            }
        }
        _ => {
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
                ActivateReq::Scroll(_, delta, orientation) => {
                    proxy.scroll(delta, orientation).await
                }
                ActivateReq::Menu(_, _) => unreachable!(),
            };
            if let Err(e) = result {
                tracing::warn!("tray: activate failed for {addr}: {e}");
            }
        }
    }
}

#[derive(Clone)]
struct TrayItemMeta {
    menu_path: Option<String>,
}

async fn add_item(
    conn: &zbus::Connection,
    addr: &str,
    icon_cache: &Arc<icon_theme::IconCache>,
    tx: &async_channel::Sender<TrayMsg>,
    ex: &async_executor::LocalExecutor<'_>,
) -> TrayItemMeta {
    let (destination, path) = parse_address(addr);
    let proxy = match StatusNotifierItemProxy::builder(conn)
        .destination(destination.to_string())
        .and_then(|b| b.path(path.to_string()))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("tray: failed to build proxy for {addr}: {e}");
                return TrayItemMeta { menu_path: None };
            }
        },
        Err(e) => {
            tracing::warn!("tray: invalid address {addr}: {e}");
            return TrayItemMeta { menu_path: None };
        }
    };

    let icon = fetch_icon(&proxy, icon_cache).await;
    let status = proxy
        .status()
        .await
        .map(|s| ItemStatus::from_str(&s))
        .unwrap_or_default();
    let menu_path = proxy
        .inner()
        .get_property::<zbus::zvariant::OwnedObjectPath>("Menu")
        .await
        .ok()
        .map(|p| p.to_string())
        .filter(|p| !p.is_empty() && p != "/");

    let _ = tx
        .send(TrayMsg::Add {
            addr: addr.to_string(),
            icon,
            status,
        })
        .await;

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

    TrayItemMeta { menu_path }
}

/// Fetch the current icon, bypassing zbus property cache by using
/// Properties.Get directly (NewIcon signal arrives before PropertiesChanged).
async fn fetch_icon(
    proxy: &StatusNotifierItemProxy<'_>,
    icon_cache: &icon_theme::IconCache,
) -> Option<TrayIcon> {
    let dest = proxy.inner().destination().to_string();
    let path = proxy.inner().path().to_string();
    tracing::debug!("tray: fetch_icon dest={dest} path={path}");
    let props = match zbus::fdo::PropertiesProxy::builder(proxy.inner().connection())
        .destination(dest.as_str())
        .and_then(|b| b.path(path.as_str()))
    {
        Ok(b) => b.build().await,
        Err(e) => {
            tracing::debug!("tray: PropertiesProxy builder failed: {e}");
            return None;
        }
    };
    let props = match props {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("tray: PropertiesProxy build failed: {e}");
            return None;
        }
    };

    // Try multiple interface names — KDE, freedesktop, and Ayatana.
    let ifaces = [
        "org.kde.StatusNotifierItem",
        "org.freedesktop.StatusNotifierItem",
        "org.ayatana.StatusNotifierItem",
    ];

    let mut pixmap_result = None;
    let mut icon_name = String::new();

    for iface_str in &ifaces {
        let iface = zbus::names::InterfaceName::from_static_str_unchecked(iface_str);

        if pixmap_result.is_none() {
            match props.get(iface.clone(), "IconPixmap").await {
                Ok(val) => {
                    if let Ok(pixmaps) = <Vec<(i32, i32, Vec<u8>)>>::try_from(val) {
                        tracing::debug!(
                            "tray: IconPixmap returned {} entries via {iface_str}",
                            pixmaps.len()
                        );
                        pixmap_result = best_pixmap_from_tuples(&pixmaps);
                    }
                }
                Err(e) => tracing::debug!("tray: IconPixmap via {iface_str}: {e}"),
            }
        }

        if icon_name.is_empty() {
            match props.get(iface, "IconName").await {
                Ok(val) => match String::try_from(val) {
                    Ok(s) if !s.is_empty() => icon_name = s,
                    _ => {}
                },
                Err(e) => tracing::debug!("tray: IconName via {iface_str}: {e}"),
            }
        }

        if pixmap_result.is_some() || !icon_name.is_empty() {
            break;
        }
    }

    if let Some(img) = pixmap_result {
        return Some(TrayIcon::Pixmap(img));
    }

    let name = icon_name;
    if name.is_empty() {
        tracing::debug!("tray: no icon available");
        return None;
    }

    tracing::debug!("tray: icon_name={name}");
    if name.starts_with('/') {
        return load_icon_file(std::path::Path::new(&name));
    }

    // Check app-specific IconThemePath first.
    let mut theme_path = String::new();
    for iface_str in &ifaces {
        let iface = zbus::names::InterfaceName::from_static_str_unchecked(iface_str);
        if let Ok(val) = props.get(iface, "IconThemePath").await {
            if let Ok(s) = String::try_from(val) {
                if !s.is_empty() {
                    theme_path = s;
                    break;
                }
            }
        }
    }
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
    let ext = path.extension()?.to_str()?;
    let format = match ext {
        "svg" | "svgz" => gpui::ImageFormat::Svg,
        "png" => gpui::ImageFormat::Png,
        // ICO/BMP/etc: decode via `image` crate to RGBA pixels.
        "ico" | "bmp" | "jpg" | "jpeg" | "gif" => {
            let mut img = image::load_from_memory(&bytes).ok()?.into_rgba8();
            // Swap R↔B for BGRA — GPUI's RenderImage expects BGRA layout.
            for pixel in img.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            let frame = image::Frame::new(img);
            return Some(TrayIcon::Pixmap(Arc::new(RenderImage::new(vec![frame]))));
        }
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

pub fn parse_address(address: &str) -> (&str, &str) {
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

    // SNI spec: IconPixmap is ARGB32 in network byte order (big-endian).
    // On little-endian hosts the in-memory byte layout per pixel is
    // `[A, R, G, B]` (i.e. src[0]=A, src[1]=R, src[2]=G, src[3]=B).
    //
    // GPUI's `RenderImage` expects BGRA byte order (see `load_icon_file`'s
    // R<->B swap for ICO), so we remap to `[B, G, R, A]`:
    //   dst[0]=B=src[3], dst[1]=G=src[2], dst[2]=R=src[1], dst[3]=A=src[0].
    let bgra = argb_be_to_bgra(pixels);

    // `image::RgbaImage` only validates buffer size; the channel order is
    // whatever we put in. We store BGRA bytes for GPUI's renderer.
    let buf = image::RgbaImage::from_raw(w, h, bgra)?;
    let frame = image::Frame::new(buf);
    Some(Arc::new(RenderImage::new(vec![frame])))
}

/// Convert big-endian ARGB32 bytes (`[A, R, G, B]` per pixel) into BGRA
/// bytes (`[B, G, R, A]` per pixel) as required by GPUI's `RenderImage`.
fn argb_be_to_bgra(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for chunk in pixels.chunks_exact(4) {
        out.extend_from_slice(&[chunk[3], chunk[2], chunk[1], chunk[0]]);
    }
    out
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
            let activate_tx_menu = self.activate_tx.clone();
            let activate_tx_scroll = self.activate_tx.clone();
            let self_tx = self.self_tx.clone();
            let addr_click = addr.clone();
            let addr_menu = addr.clone();
            let addr_scroll = addr.clone();

            let base = div()
                .id(gpui::ElementId::Name(addr.clone().into()))
                .cursor_pointer()
                .rounded_sm()
                .hover(|s| s.bg(theme::surface_hover()))
                .p(gpui::px(2.0))
                .on_mouse_down(MouseButton::Left, move |_, _, _cx| {
                    // Close any open menu first.
                    let _ = self_tx.try_send(TrayMsg::CloseMenu);
                    let _ = activate_tx.try_send(ActivateReq::Default(addr_click.clone()));
                })
                .on_mouse_down(MouseButton::Right, move |ev, _, _cx| {
                    let x: f32 = ev.position.x.into();
                    let _ = activate_tx_menu.try_send(ActivateReq::Menu(addr_menu.clone(), x));
                })
                .on_scroll_wheel(move |ev, _window, _cx| {
                    // SNI Scroll spec: delta is a signed int; sign convention
                    // is up/left = negative. Pick the dominant axis so a
                    // diagonal gesture doesn't fire twice.
                    let (dx, dy) = match ev.delta {
                        gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                        gpui::ScrollDelta::Lines(p) => (p.x * 10.0, p.y * 10.0),
                    };
                    let (delta, orientation) = if dy.abs() >= dx.abs() {
                        // GPUI/wheel: positive dy = scroll down → negative SNI delta.
                        (-dy.round() as i32, "vertical")
                    } else {
                        (-dx.round() as i32, "horizontal")
                    };
                    if delta == 0 {
                        return;
                    }
                    let _ = activate_tx_scroll.try_send(ActivateReq::Scroll(
                        addr_scroll.clone(),
                        delta,
                        orientation,
                    ));
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

#[cfg(test)]
mod tests {
    use super::argb_be_to_bgra;

    #[test]
    fn argb_be_to_bgra_maps_single_pixel() {
        // SNI big-endian ARGB32 byte layout: [A, R, G, B].
        // Pixel: A=0x12, R=0x34, G=0x56, B=0x78
        let src = [0x12, 0x34, 0x56, 0x78];
        // Expected BGRA: [B, G, R, A] = [0x78, 0x56, 0x34, 0x12]
        let dst = argb_be_to_bgra(&src);
        assert_eq!(dst, vec![0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn argb_be_to_bgra_maps_multiple_pixels() {
        // Two pixels: opaque red, then half-transparent green.
        let src = [
            0xFF, 0xFF, 0x00, 0x00, // A=FF, R=FF, G=00, B=00  (opaque red)
            0x80, 0x00, 0xFF, 0x00, // A=80, R=00, G=FF, B=00  (half green)
        ];
        let dst = argb_be_to_bgra(&src);
        assert_eq!(
            dst,
            vec![
                0x00, 0x00, 0xFF, 0xFF, // B=00, G=00, R=FF, A=FF
                0x00, 0xFF, 0x00, 0x80, // B=00, G=FF, R=00, A=80
            ]
        );
    }

    #[test]
    fn argb_be_to_bgra_preserves_length() {
        let src = vec![0u8; 16];
        assert_eq!(argb_be_to_bgra(&src).len(), 16);
    }

    #[test]
    fn argb_be_to_bgra_ignores_trailing_partial_pixel() {
        // chunks_exact drops any trailing bytes that don't form a full pixel.
        let src = [0x12, 0x34, 0x56, 0x78, 0xAA, 0xBB];
        let dst = argb_be_to_bgra(&src);
        assert_eq!(dst, vec![0x78, 0x56, 0x34, 0x12]);
    }
}
