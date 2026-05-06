use gpui::{
    div, img, px, AnyView, App, AppContext, Context, ImageSource, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, RenderImage, SharedString, StatefulInteractiveElement,
    Styled, Window,
};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use ztheme::Theme;

/// Shared, lock-protected hex string for the active theme's foreground.
/// The tray-sni thread reads this whenever it needs to recolor a symbolic
/// SVG (`load_icon_file` and friends); the GPUI thread updates it via
/// `cx.observe_global::<Theme>` when the user picks a new palette.
///
/// We use a small `RwLock<&'static str>` rather than an atomic because
/// the value is only ever one of two static literals from `ztheme::fg_hex`,
/// so the read path is the trivial "clone the `&'static str`". Lock
/// contention is negligible — writes happen at most once per theme switch
/// and reads happen on icon load (already a slow path).
pub(crate) type FgHexState = Arc<RwLock<&'static str>>;

#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    #[error("dbus: {0}")]
    Dbus(#[from] zbus::Error),
    #[error("fdo: {0}")]
    Fdo(#[from] zbus::fdo::Error),
    #[error("invalid host name: {0}")]
    InvalidName(String),
}

type Result<T> = std::result::Result<T, TrayError>;

pub struct TrayModule {
    items: BTreeMap<String, TrayItem>,
    activate_tx: async_channel::Sender<ActivateReq>,
    menu_click_tx: async_channel::Sender<tray_menu::MenuClickReq>,
    /// For sending CloseMenu from popup back to ourselves.
    self_tx: async_channel::Sender<TrayMsg>,
    display_id: Option<gpui::DisplayId>,
    /// Currently-open context menu: the item address and its popup handle
    /// are kept together to prevent them drifting out of sync.
    open_menu: Option<(String, gpui::WindowHandle<tray_menu::TrayMenuPopup>)>,
}

struct TrayItem {
    icon: Option<TrayIcon>,
    status: ItemStatus,
    tooltip: Option<Tooltip>,
}

/// Parsed subset of the SNI `ToolTip` property `(s icon_name, a(iiay) icon_pixmap,
/// s title, s description)`. We only surface the text for now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tooltip {
    pub title: String,
    pub description: String,
}

impl Tooltip {
    fn is_empty(&self) -> bool {
        self.title.is_empty() && self.description.is_empty()
    }
}

/// Icon data — either pre-decoded pixels (from IconPixmap) or an encoded
/// image file (PNG/SVG) that GPUI will decode at render time.
#[derive(Clone)]
pub(crate) enum TrayIcon {
    Pixmap(Arc<RenderImage>),
    File(Arc<gpui::Image>),
}

impl TrayIcon {
    /// Cheap identity check for change detection. Tray apps often re-push
    /// the same icon on every status tick; if the underlying `Arc` hasn't
    /// changed we can skip the GPUI re-render entirely.
    fn ptr_eq(a: &Option<Self>, b: &Option<Self>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(Self::Pixmap(l)), Some(Self::Pixmap(r))) => Arc::ptr_eq(l, r),
            (Some(Self::File(l)), Some(Self::File(r))) => Arc::ptr_eq(l, r),
            _ => false,
        }
    }
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
        tooltip: Option<Tooltip>,
    },
    CloseMenu,
    UpdateIcon(String, Option<TrayIcon>),
    UpdateStatus(String, ItemStatus),
    UpdateTooltip(String, Option<Tooltip>),
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

impl ActivateReq {
    fn addr(&self) -> &str {
        match self {
            ActivateReq::Default(a)
            | ActivateReq::Secondary(a)
            | ActivateReq::Menu(a, _)
            | ActivateReq::Scroll(a, _, _) => a.as_str(),
        }
    }
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

    #[zbus(signal)]
    fn new_tool_tip(&self) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Module implementation
// ---------------------------------------------------------------------------

impl TrayModule {
    fn close_menu(&mut self, cx: &mut gpui::App) {
        if let Some((_, handle)) = self.open_menu.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    pub fn new(display_id: Option<gpui::DisplayId>, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded::<TrayMsg>(32);
        let self_tx = tx.clone();
        let (activate_tx, activate_rx) = async_channel::bounded::<ActivateReq>(8);
        let (menu_click_tx, menu_click_rx) = async_channel::bounded::<tray_menu::MenuClickReq>(8);

        // Bridge ztheme's global `Theme` to the tray-sni thread:
        // 1) `fg_hex_state` exposes the current symbolic-icon recolor target
        //    so newly loaded SVGs paint correctly out of the gate.
        // 2) `theme_changed_tx` notifies the SNI session that already-loaded
        //    icons need to be re-fetched with the new fg, since their
        //    recolored bytes are baked into `gpui::Image` at load time.
        let initial_fg = ztheme::fg_hex(cx.global::<Theme>());
        let fg_hex_state: FgHexState = Arc::new(RwLock::new(initial_fg));
        let (theme_changed_tx, theme_changed_rx) = async_channel::bounded::<()>(4);

        let observer_state = fg_hex_state.clone();
        let observer_tx = theme_changed_tx.clone();
        cx.observe_global::<Theme>(move |_, cx| {
            let new_fg = ztheme::fg_hex(cx.global::<Theme>());
            // Skip the refetch broadcast if the fg didn't actually change
            // (theme observers can fire on unrelated state mutations).
            let changed = {
                let mut guard = observer_state.write().unwrap_or_else(|p| p.into_inner());
                if *guard == new_fg {
                    false
                } else {
                    *guard = new_fg;
                    true
                }
            };
            if changed {
                // Bounded channel; if the SNI thread is backlogged we
                // drop the extra signal — a single refetch sweep will
                // pick up the latest fg via `fg_hex_state` anyway.
                let _ = observer_tx.try_send(());
            }
            cx.notify();
        })
        .detach();

        cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this
                    .update(cx, |m, cx| match msg {
                        TrayMsg::Add {
                            addr,
                            icon,
                            status,
                            tooltip,
                        } => {
                            m.items.insert(
                                addr,
                                TrayItem {
                                    icon,
                                    status,
                                    tooltip,
                                },
                            );
                            cx.notify();
                        }
                        TrayMsg::UpdateIcon(addr, icon) => {
                            if let Some(item) = m.items.get_mut(&addr) {
                                if !TrayIcon::ptr_eq(&item.icon, &icon) {
                                    item.icon = icon;
                                    cx.notify();
                                }
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
                        TrayMsg::UpdateTooltip(addr, tooltip) => {
                            if let Some(item) = m.items.get_mut(&addr) {
                                if item.tooltip != tooltip {
                                    item.tooltip = tooltip;
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
                            if m.open_menu.as_ref().map(|(a, _)| a.as_str()) == Some(&addr) {
                                m.close_menu(cx);
                                return;
                            }
                            m.close_menu(cx);
                            if let Some(handle) = tray_menu::open_menu_popup(
                                cx,
                                items,
                                addr.clone(),
                                menu_path,
                                m.menu_click_tx.clone(),
                                m.self_tx.clone(),
                                m.display_id,
                                click_x,
                            ) {
                                m.open_menu = Some((addr, handle));
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

        let sni_fg_state = fg_hex_state.clone();
        std::thread::Builder::new()
            .name("tray-sni".into())
            .spawn(move || {
                async_io::block_on(run_sni_host(
                    tx,
                    activate_rx,
                    menu_click_rx,
                    sni_fg_state,
                    theme_changed_rx,
                ));
            })
            .expect("spawn tray thread");

        TrayModule {
            items: BTreeMap::new(),
            activate_tx,
            menu_click_tx,
            self_tx,
            display_id,
            open_menu: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal StatusNotifierWatcher server
// ---------------------------------------------------------------------------

const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const ITEM_OBJECT: &str = "/StatusNotifierItem";

/// Interface names to try when reading StatusNotifierItem properties.
/// Different toolkits publish items under different vendor prefixes — we
/// try KDE first (most common), then the freedesktop draft, then Ayatana.
const SNI_INTERFACES: [&str; 3] = [
    "org.kde.StatusNotifierItem",
    "org.freedesktop.StatusNotifierItem",
    "org.ayatana.StatusNotifierItem",
];

/// Fetch one SNI property, trying each known interface name in order.
/// Returns the first value that both reads successfully and converts to `T`.
async fn get_sni_prop<T>(props: &zbus::fdo::PropertiesProxy<'_>, name: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
{
    for iface_str in &SNI_INTERFACES {
        let iface = zbus::names::InterfaceName::from_static_str_unchecked(iface_str);
        let Ok(val) = props.get(iface, name).await else {
            continue;
        };
        if let Ok(out) = T::try_from(val) {
            return Some(out);
        }
    }
    None
}

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

async fn start_watcher(conn: &zbus::Connection) -> Result<WatcherHandle> {
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
) -> Result<()> {
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
    fg_hex_state: FgHexState,
    theme_changed_rx: async_channel::Receiver<()>,
) {
    let mut delay_ms: u64 = 1000;
    loop {
        match run_sni_session(
            &tx,
            &activate_rx,
            &menu_click_rx,
            &fg_hex_state,
            &theme_changed_rx,
        )
        .await
        {
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
    fg_hex_state: &FgHexState,
    theme_changed_rx: &async_channel::Receiver<()>,
) -> Result<()> {
    let conn = zbus::Connection::session().await?;

    // Start our Watcher — returns a channel for item registrations
    // (D-Bus doesn't loop signals back to the same connection).
    let watcher_handle = match start_watcher(&conn).await {
        Ok(handle) => Some(handle),
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
        .map_err(|e: zbus::names::Error| TrayError::InvalidName(e.to_string()))?;
    conn.request_name(&host_wellknown).await?;
    watcher.register_status_notifier_host(&host_name).await?;
    tracing::info!("tray: registered as SNI host");

    let icon_cache = Arc::new(icon_theme::IconCache::new(&[
        "apps", "status", "devices", "actions",
    ]));

    let ex = async_executor::LocalExecutor::new();
    let item_metas = std::cell::RefCell::new(BTreeMap::<String, TrayItemMeta>::new());
    // Per-item refetch trigger. On theme change we fan out a `()` to every
    // entry so each NewIcon watcher re-issues `fetch_icon` with the new
    // fg_hex (the recolored bytes are baked into `gpui::Image` at load
    // time, so a simple `cx.notify()` won't repaint correctly).
    let item_refetch =
        std::cell::RefCell::new(BTreeMap::<String, async_channel::Sender<()>>::new());

    // Spawn the owner-lost cleanup task on the session-local executor so it
    // is cancelled automatically when this session ends (ex is dropped on
    // reconnect), preventing a thread leak across reconnect cycles.
    let watcher_event_rx = match watcher_handle {
        Some(handle) => {
            let cleanup_conn = conn.clone();
            let cleanup_state = handle.state.clone();
            let cleanup_tx = handle.event_tx.clone();
            ex.spawn(async move {
                if let Err(e) =
                    watch_name_owner_changed(cleanup_conn, cleanup_state, cleanup_tx).await
                {
                    tracing::warn!("tray: NameOwnerChanged watcher ended: {e}");
                }
            })
            .detach();
            Some(handle.event_rx)
        }
        None => None,
    };

    match watcher.registered_status_notifier_items().await {
        Ok(items) => {
            tracing::info!("tray: found {} initial item(s)", items.len());
            for addr in items {
                let (meta, refetch_tx) =
                    add_item(&conn, &addr, &icon_cache, tx, &ex, fg_hex_state).await;
                item_metas.borrow_mut().insert(addr.clone(), meta);
                item_refetch.borrow_mut().insert(addr, refetch_tx);
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
                    futures_lite::future::or(
                        async {
                            if let Some(ref rx) = watcher_event_rx {
                                if let Ok(event) = rx.recv().await {
                                    match event {
                                        WatcherEvent::ItemAdded(addr) => {
                                            if !item_metas.borrow().contains_key(&addr) {
                                                tracing::debug!("tray item added: {addr}");
                                                let (meta, refetch_tx) = add_item(
                                                    &conn,
                                                    &addr,
                                                    &icon_cache,
                                                    tx,
                                                    &ex,
                                                    fg_hex_state,
                                                )
                                                .await;
                                                item_metas.borrow_mut().insert(addr.clone(), meta);
                                                item_refetch.borrow_mut().insert(addr, refetch_tx);
                                            }
                                        }
                                        WatcherEvent::ItemRemoved(addr) => {
                                            tracing::debug!("tray item removed: {addr}");
                                            item_metas.borrow_mut().remove(&addr);
                                            item_refetch.borrow_mut().remove(&addr);
                                            let _ = tx.send(TrayMsg::Remove(addr)).await;
                                        }
                                    }
                                }
                            } else {
                                futures_lite::future::pending::<()>().await;
                            }
                        },
                        // Theme switched on the GPUI thread — fan out a
                        // refetch trigger to every per-item NewIcon worker
                        // so they re-issue `fetch_icon` and pick up the
                        // updated `fg_hex` for SVG recoloring.
                        async {
                            if theme_changed_rx.recv().await.is_ok() {
                                tracing::debug!(
                                    "tray: theme changed, refetching {} icon(s)",
                                    item_refetch.borrow().len()
                                );
                                let triggers: Vec<async_channel::Sender<()>> =
                                    item_refetch.borrow().values().cloned().collect();
                                for trigger in triggers {
                                    let _ = trigger.try_send(());
                                }
                            }
                        },
                    ),
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
                        let meta = item_metas.borrow().get(req.addr()).cloned();
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
    let addr = req.addr();

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
    /// Cached `IconThemePath` property — per the SNI spec this is a
    /// per-item hint that does not change over an item's lifetime, so we
    /// fetch it once at registration and reuse on every NewIcon signal
    /// instead of re-issuing a D-Bus round trip each time. Read through
    /// a clone captured by the NewIcon watcher task, so the field itself
    /// looks unused to the dead-code analyser.
    #[allow(dead_code)]
    icon_theme_path: Option<String>,
}

async fn add_item(
    conn: &zbus::Connection,
    addr: &str,
    icon_cache: &Arc<icon_theme::IconCache>,
    tx: &async_channel::Sender<TrayMsg>,
    ex: &async_executor::LocalExecutor<'_>,
    fg_hex_state: &FgHexState,
) -> (TrayItemMeta, async_channel::Sender<()>) {
    let empty = TrayItemMeta {
        menu_path: None,
        icon_theme_path: None,
    };
    // Per-item refetch trigger. Capacity 4 so a burst of theme switches
    // collapses into "do at most one extra refetch" instead of dropping
    // the latest one.
    let (refetch_tx, refetch_rx) = async_channel::bounded::<()>(4);
    let (destination, path) = parse_address(addr);
    let proxy = match StatusNotifierItemProxy::builder(conn)
        .destination(destination.to_string())
        .and_then(|b| b.path(path.to_string()))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("tray: failed to build proxy for {addr}: {e}");
                return (empty, refetch_tx);
            }
        },
        Err(e) => {
            tracing::warn!("tray: invalid address {addr}: {e}");
            return (empty, refetch_tx);
        }
    };

    let initial_fg = current_fg_hex(fg_hex_state);
    // Batch-fetch every property we need in one D-Bus round trip
    // (Properties.GetAll) instead of 4–5 sequential Get calls.
    let (icon, status, menu_path, tooltip, icon_theme_path) =
        fetch_all_item_props(&proxy, icon_cache, initial_fg).await;

    let _ = tx
        .send(TrayMsg::Add {
            addr: addr.to_string(),
            icon,
            status,
            tooltip,
        })
        .await;

    // Spawn NewIcon watcher. Listens on both the SNI `NewIcon` signal and
    // a local refetch trigger that the session loop pushes when the user
    // switches theme — both paths share the same fetch+update plumbing.
    {
        let tx = tx.clone();
        let addr = addr.to_string();
        let icon_cache = icon_cache.clone();
        let proxy_inner = proxy.inner().clone();
        let cached_theme_path = icon_theme_path.clone();
        let fg_state = fg_hex_state.clone();
        ex.spawn(async move {
            use futures_lite::StreamExt;
            let proxy = StatusNotifierItemProxy::from(proxy_inner);
            let Ok(mut stream) = proxy.receive_new_icon().await else {
                tracing::debug!("tray: failed to subscribe NewIcon for {addr}");
                return;
            };
            tracing::debug!("tray: watching NewIcon for {addr}");
            loop {
                // Race the SNI NewIcon signal against the theme-change
                // refetch trigger — both produce the same downstream
                // `UpdateIcon` message, so the body below is shared.
                let trigger = futures_lite::future::or(
                    async {
                        if stream.next().await.is_some() {
                            tracing::debug!("tray: NewIcon signal for {addr}");
                            Some(())
                        } else {
                            None
                        }
                    },
                    async {
                        if refetch_rx.recv().await.is_ok() {
                            tracing::debug!("tray: theme refetch for {addr}");
                            Some(())
                        } else {
                            None
                        }
                    },
                )
                .await;
                if trigger.is_none() {
                    break;
                }
                let fg = current_fg_hex(&fg_state);
                let icon = fetch_icon(&proxy, &icon_cache, cached_theme_path.as_deref(), fg).await;
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

    // Spawn NewToolTip watcher.
    {
        let tx = tx.clone();
        let addr = addr.to_string();
        let proxy_inner = proxy.inner().clone();
        ex.spawn(async move {
            use futures_lite::StreamExt;
            let proxy = StatusNotifierItemProxy::from(proxy_inner);
            let Ok(mut stream) = proxy.receive_new_tool_tip().await else {
                return;
            };
            while stream.next().await.is_some() {
                let tooltip = fetch_tooltip(&proxy).await;
                if tx
                    .send(TrayMsg::UpdateTooltip(addr.clone(), tooltip))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    (
        TrayItemMeta {
            menu_path,
            icon_theme_path,
        },
        refetch_tx,
    )
}

/// Read the current foreground hex from the shared state. Falls back to the
/// mocha literal if the lock is poisoned — the worst case is one icon
/// rendered with the wrong recolor target, which the next theme observer
/// fire will correct.
fn current_fg_hex(state: &FgHexState) -> &'static str {
    match state.read() {
        Ok(g) => *g,
        Err(p) => *p.into_inner(),
    }
}

/// Batch-fetch every property we need about a newly-registered tray item.
///
/// Tries `Properties.GetAll` for each candidate SNI interface in order;
/// the first one that yields a non-empty property map wins. This replaces
/// what used to be ~10 sequential D-Bus calls (per-property × per-interface
/// probing) with typically a single round trip. If no interface returns
/// anything usable, we fall back to per-property probing for the icon so
/// apps that implement `Get` but not `GetAll` still render.
async fn fetch_all_item_props(
    proxy: &StatusNotifierItemProxy<'_>,
    icon_cache: &icon_theme::IconCache,
    fg_hex: &str,
) -> (
    Option<TrayIcon>,
    ItemStatus,
    Option<String>,
    Option<Tooltip>,
    Option<String>,
) {
    use zbus::zvariant::{OwnedValue, Value};

    let dest = proxy.inner().destination().to_string();
    let path = proxy.inner().path().to_string();
    let builder = match zbus::fdo::PropertiesProxy::builder(proxy.inner().connection())
        .destination(dest.as_str())
        .and_then(|b| b.path(path.as_str()))
    {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!("tray: PropertiesProxy builder failed: {e}");
            return (None, ItemStatus::default(), None, None, None);
        }
    };
    let props = match builder.build().await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("tray: PropertiesProxy build failed: {e}");
            return (None, ItemStatus::default(), None, None, None);
        }
    };

    let mut all: std::collections::HashMap<String, OwnedValue> = Default::default();
    for iface_str in &SNI_INTERFACES {
        let iface = zbus::names::InterfaceName::from_static_str_unchecked(iface_str);
        match props.get_all(iface).await {
            Ok(map) if !map.is_empty() => {
                all = map;
                break;
            }
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!("tray: GetAll via {iface_str}: {e}");
                continue;
            }
        }
    }

    let take = |m: &mut std::collections::HashMap<String, OwnedValue>, key: &str| m.remove(key);

    let icon_name = take(&mut all, "IconName")
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default();
    let pixmap_result = take(&mut all, "IconPixmap")
        .and_then(|v| <Vec<(i32, i32, Vec<u8>)>>::try_from(v).ok())
        .and_then(|p| {
            tracing::debug!("tray: IconPixmap returned {} entries", p.len());
            best_pixmap_from_tuples(&p)
        });
    let icon_theme_path = take(&mut all, "IconThemePath")
        .and_then(|v| String::try_from(v).ok())
        .filter(|s| !s.is_empty());
    let status = take(&mut all, "Status")
        .and_then(|v| String::try_from(v).ok())
        .map(|s| ItemStatus::from_str(&s))
        .unwrap_or_default();
    let menu_path = take(&mut all, "Menu")
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
        .map(|p| p.to_string())
        .filter(|p| !p.is_empty() && p != "/");
    let tooltip = take(&mut all, "ToolTip").and_then(|v| {
        let inner: Value<'_> = v.into();
        let Value::Structure(s) = inner else {
            return None;
        };
        tooltip_from_structure(&s)
    });

    // If GetAll didn't yield any of the icon bits (some apps only
    // implement Get), fall back to the per-property probe so we still
    // surface an icon rather than rendering a blank slot.
    let icon = if pixmap_result.is_some() || !icon_name.is_empty() {
        resolve_icon(
            pixmap_result,
            &icon_name,
            icon_theme_path.as_deref(),
            icon_cache,
            fg_hex,
        )
    } else {
        fetch_icon(proxy, icon_cache, icon_theme_path.as_deref(), fg_hex).await
    };

    (icon, status, menu_path, tooltip, icon_theme_path)
}

/// Fetch the SNI `ToolTip` property via Properties.Get, trying each
/// known interface name. Signature is `(s icon_name, a(iiay) icon_pixmap,
/// s title, s description)` — we only read the two string fields.
async fn fetch_tooltip(proxy: &StatusNotifierItemProxy<'_>) -> Option<Tooltip> {
    use zbus::zvariant::Value;

    let dest = proxy.inner().destination().to_string();
    let path = proxy.inner().path().to_string();
    let props = match zbus::fdo::PropertiesProxy::builder(proxy.inner().connection())
        .destination(dest.as_str())
        .and_then(|b| b.path(path.as_str()))
    {
        Ok(b) => b.build().await.ok()?,
        Err(_) => return None,
    };

    let val = get_sni_prop::<zbus::zvariant::OwnedValue>(&props, "ToolTip").await?;
    // Unwrap one level: Properties.Get returns Value::Value(inner).
    let inner: Value<'_> = val.into();
    let Value::Structure(structure) = inner else {
        return None;
    };
    tooltip_from_structure(&structure)
}

fn tooltip_from_structure(s: &zbus::zvariant::Structure<'_>) -> Option<Tooltip> {
    let fields = s.fields();
    // Expect 4 fields: icon_name(s), icon_pixmap(a(iiay)), title(s), description(s).
    if fields.len() < 4 {
        return None;
    }
    let title = field_as_string(&fields[2]).unwrap_or_default();
    let description = field_as_string(&fields[3]).unwrap_or_default();
    let tt = Tooltip { title, description };
    if tt.is_empty() {
        None
    } else {
        Some(tt)
    }
}

fn field_as_string(v: &zbus::zvariant::Value<'_>) -> Option<String> {
    use zbus::zvariant::Value;
    match v {
        Value::Str(s) => Some(s.as_str().to_owned()),
        _ => None,
    }
}

/// Fetch the current icon, bypassing zbus property cache by using
/// Properties.Get directly (NewIcon signal arrives before PropertiesChanged).
///
/// `theme_path` is the cached `IconThemePath` for this item (immutable for
/// the lifetime of a tray item), so we don't re-fetch it on every NewIcon
/// signal.
async fn fetch_icon(
    proxy: &StatusNotifierItemProxy<'_>,
    icon_cache: &icon_theme::IconCache,
    theme_path: Option<&str>,
    fg_hex: &str,
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

    let pixmap_result = get_sni_prop::<Vec<(i32, i32, Vec<u8>)>>(&props, "IconPixmap")
        .await
        .and_then(|pixmaps| {
            tracing::debug!("tray: IconPixmap returned {} entries", pixmaps.len());
            best_pixmap_from_tuples(&pixmaps)
        });
    let icon_name = get_sni_prop::<String>(&props, "IconName")
        .await
        .unwrap_or_default();

    resolve_icon(pixmap_result, &icon_name, theme_path, icon_cache, fg_hex)
}

/// Resolve the best icon source from already-fetched properties. Extracted
/// so the batch-fetch path in `add_item` can reuse icon selection logic
/// without paying for a second round trip.
fn resolve_icon(
    pixmap_result: Option<Arc<RenderImage>>,
    icon_name: &str,
    theme_path: Option<&str>,
    icon_cache: &icon_theme::IconCache,
    fg_hex: &str,
) -> Option<TrayIcon> {
    if let Some(img) = pixmap_result {
        return Some(TrayIcon::Pixmap(img));
    }

    if icon_name.is_empty() {
        tracing::debug!("tray: no icon available");
        return None;
    }

    tracing::debug!("tray: icon_name={icon_name}");
    if icon_name.starts_with('/') {
        return load_icon_file(std::path::Path::new(icon_name), fg_hex);
    }

    // Check app-specific IconThemePath first.
    if let Some(theme_path) = theme_path.filter(|s| !s.is_empty()) {
        let dir = std::path::Path::new(theme_path);
        for ext in &["svg", "svgz", "png"] {
            let candidate = dir.join(format!("{icon_name}.{ext}"));
            if candidate.exists() {
                tracing::debug!(
                    "tray: icon_name={icon_name} via theme_path: {}",
                    candidate.display()
                );
                return load_icon_file(&candidate, fg_hex);
            }
        }
        // Also check subdirectories (e.g. hicolor/48x48/status/)
        if dir.is_dir() {
            if let Some(icon) = find_icon_recursive(dir, icon_name, 3) {
                tracing::debug!(
                    "tray: icon_name={icon_name} via theme_path subdir: {}",
                    icon.display()
                );
                return load_icon_file(&icon, fg_hex);
            }
        }
    }

    let path = icon_cache.lookup(icon_name)?;
    tracing::debug!(
        "tray: icon_name={icon_name} via system theme: {}",
        path.display()
    );
    load_icon_file(path, fg_hex)
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

fn load_icon_file(path: &std::path::Path, fg_hex: &str) -> Option<TrayIcon> {
    // Reject oversized icon files before reading/decoding — untrusted tray
    // apps (or a malicious `IconThemePath`) could point at a huge file and
    // block the SNI thread (all tray updates are serialized).
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_ICON_BYTES as u64 {
            tracing::warn!(
                "tray icon file too large: {} bytes at {}",
                meta.len(),
                path.display()
            );
            return None;
        }
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > MAX_ICON_BYTES {
        tracing::warn!(
            "tray icon file too large: {} bytes at {}",
            bytes.len(),
            path.display()
        );
        return None;
    }
    let ext = path.extension()?.to_str()?;
    let format = match ext {
        "svg" | "svgz" => gpui::ImageFormat::Svg,
        "png" => gpui::ImageFormat::Png,
        // ICO/BMP/etc: decode via `image` crate to RGBA pixels.
        "ico" | "bmp" | "jpg" | "jpeg" | "gif" => {
            let mut img = match image::load_from_memory(&bytes) {
                Ok(img) => img.into_rgba8(),
                Err(err) => {
                    tracing::warn!("tray icon decode failed for {}: {err}", path.display());
                    return None;
                }
            };
            // Swap R↔B for BGRA — GPUI's RenderImage expects BGRA layout.
            for pixel in img.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            let frame = image::Frame::new(img);
            return Some(TrayIcon::Pixmap(Arc::new(RenderImage::new(vec![frame]))));
        }
        _ => return None,
    };
    // Recolor symbolic SVG icons so the strokes contrast against the
    // current panel — Breeze/KDE icons ship dark fills that vanish on a
    // dark theme bg and look muddy on a light one. Target color tracks
    // the active `Theme.fg` via `ztheme::fg_hex(...)`.
    let bytes = if format == gpui::ImageFormat::Svg {
        recolor_svg_for_panel(bytes, fg_hex)
    } else {
        bytes
    };

    Some(TrayIcon::File(Arc::new(gpui::Image {
        format,
        bytes,
        id: NEXT_IMAGE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    })))
}

/// Replace common dark fill colors used by KDE/GNOME symbolic SVGs with the
/// supplied panel foreground hex (e.g. `"#cdd6f4"` for mocha, `"#4c4f69"`
/// for latte). `fg_hex` is taken as a parameter rather than read off a
/// global so `load_icon_file` stays a pure function — the caller threads
/// the active theme's fg through.
fn recolor_svg_for_panel(bytes: Vec<u8>, fg_hex: &str) -> Vec<u8> {
    // Common dark colors used by Breeze / KDE symbolic icons.
    const DARK_COLORS: &[&str] = &["#31363b", "#232629", "#4d4d4d", "#000000"];

    let mut svg = String::from_utf8_lossy(&bytes).into_owned();
    for dark in DARK_COLORS {
        svg = svg.replace(dark, fg_hex);
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

/// Maximum accepted dimension for a single tray icon side. Real tray icons
/// are rendered at ~18px; 512 is already an order of magnitude larger than
/// anything sensible. Rejecting larger values guards against integer
/// overflow in `w * h * 4` and OOM from adversarial D-Bus peers.
const MAX_ICON_DIM: i32 = 512;

/// Hard cap on icon file size read from disk (4 MiB). Real tray icon assets
/// are a few KiB; anything past this is almost certainly a malformed or
/// malicious file pointed at by `IconThemePath`.
const MAX_ICON_BYTES: usize = 4 * 1024 * 1024;

fn best_pixmap_from_tuples(pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<Arc<RenderImage>> {
    pixmaps
        .iter()
        .max_by_key(|(w, h, _)| {
            // Clamp negatives to 0 and widen to u64 to avoid i32 overflow
            // when an adversarial peer sends huge dimensions.
            let w = u64::from((*w).max(0) as u32);
            let h = u64::from((*h).max(0) as u32);
            w * h
        })
        .and_then(|(w, h, pixels)| render_argb_pixmap(*w, *h, pixels))
}

fn render_argb_pixmap(w: i32, h: i32, pixels: &[u8]) -> Option<Arc<RenderImage>> {
    if w <= 0 || h <= 0 {
        tracing::warn!("tray icon pixmap has non-positive dimensions: {w}x{h}");
        return None;
    }
    if w > MAX_ICON_DIM || h > MAX_ICON_DIM {
        tracing::warn!("tray icon pixmap dimensions exceed cap ({MAX_ICON_DIM}): {w}x{h}");
        return None;
    }
    let w = w as u32;
    let h = h as u32;
    // Compute expected byte length with overflow checks — a malicious peer
    // could otherwise wrap u32 and bypass the length check below.
    let expected = match w.checked_mul(h).and_then(|n| n.checked_mul(4)) {
        Some(n) => n as usize,
        None => {
            tracing::warn!("tray icon pixmap size overflow: {w}x{h}");
            return None;
        }
    };
    if pixels.len() != expected {
        tracing::warn!(
            "tray icon pixmap size mismatch: {w}x{h} but {} bytes (expected {expected})",
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

/// Minimal floating tooltip view used for tray hover text. SNI tooltips are
/// plain strings (title + optional description) — no markup — so we render
/// them as a small pill with panel colors matching the rest of the bar.
struct SimpleTooltip {
    title: SharedString,
    description: SharedString,
}

impl Render for SimpleTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.global::<Theme>();
        let mut col = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .text_color(t.fg)
            .text_size(px(12.0));

        if !self.title.is_empty() {
            col = col.child(div().child(self.title.clone()));
        }
        if !self.description.is_empty() {
            col = col.child(div().text_color(t.fg_dim).child(self.description.clone()));
        }
        col
    }
}

fn build_tooltip_view(tooltip: &Tooltip, cx: &mut App) -> AnyView {
    let title: SharedString = tooltip.title.clone().into();
    let description: SharedString = tooltip.description.clone().into();
    cx.new(|_| SimpleTooltip { title, description }).into()
}

impl Render for TrayModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.global::<Theme>();
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
                .hover(move |s| s.bg(t.surface_hover))
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

            // Attach hover tooltip when ToolTip property carries any text.
            let base = if let Some(ref tt) = item.tooltip {
                if tt.is_empty() {
                    base
                } else {
                    let tt = tt.clone();
                    base.tooltip(move |_window, cx| build_tooltip_view(&tt, cx))
                }
            } else {
                base
            };

            let child = if let Some(ref icon) = item.icon {
                base.child(match icon {
                    TrayIcon::Pixmap(r) => img(ImageSource::Render(r.clone())).size(ICON_SIZE),
                    TrayIcon::File(i) => img(ImageSource::Image(i.clone())).size(ICON_SIZE),
                })
            } else {
                base.size(ICON_SIZE).bg(t.fg_dim).rounded_full()
            };

            // NeedsAttention: subtle visual hint.
            let child = if item.status == ItemStatus::NeedsAttention {
                child.bg(t.accent_soft)
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

    use super::{best_pixmap_from_tuples, render_argb_pixmap, MAX_ICON_DIM};

    #[test]
    fn render_argb_pixmap_rejects_negative_dimensions() {
        assert!(render_argb_pixmap(-1, 10, &[0u8; 40]).is_none());
        assert!(render_argb_pixmap(10, 0, &[0u8; 40]).is_none());
    }

    #[test]
    fn render_argb_pixmap_rejects_oversize_dimensions() {
        // Exceeds MAX_ICON_DIM (512) — must be rejected.
        let side = MAX_ICON_DIM + 1;
        let bytes = vec![0u8; (side as usize) * (side as usize) * 4];
        assert!(render_argb_pixmap(side, side, &bytes).is_none());
    }

    #[test]
    fn render_argb_pixmap_rejects_overflow_dimensions() {
        // Even if the caller somehow slipped past the cap, multiplications
        // must not overflow silently. Use a dimension that would wrap u32
        // when squared*4. (Guarded already by MAX_ICON_DIM, so we just
        // confirm rejection rather than panic.)
        let huge = 0x10000i32; // 65536
        assert!(render_argb_pixmap(huge, huge, &[]).is_none());
    }

    #[test]
    fn render_argb_pixmap_rejects_wrong_buffer_length() {
        // 2x2 pixels * 4 bytes = 16, but only 8 supplied.
        assert!(render_argb_pixmap(2, 2, &[0u8; 8]).is_none());
    }

    #[test]
    fn best_pixmap_from_tuples_survives_i32_overflow_key() {
        // w*h as i32 would overflow for 0x8000 * 0x8000; the sorter must
        // not panic. Buffer lengths are deliberately wrong so the chosen
        // pixmap is rejected by render_argb_pixmap — we only care the
        // key computation doesn't overflow.
        let pixmaps = vec![(0x8000i32, 0x8000i32, vec![]), (-1i32, -1i32, vec![])];
        assert!(best_pixmap_from_tuples(&pixmaps).is_none());
    }
}
