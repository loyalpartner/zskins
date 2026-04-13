//! Netlink (rtnetlink) worker for live interface information.
//!
//! Uses raw libc netlink syscalls — no `ip(8)`, `getifaddrs`, or `/sys/class/net`.
//! Two NETLINK_ROUTE sockets are used:
//!   * a dump socket for `RTM_GETLINK` / `RTM_GETADDR` requests.
//!   * a multicast socket subscribed to `RTNLGRP_LINK`, `RTNLGRP_IPV4_IFADDR`,
//!     `RTNLGRP_IPV6_IFADDR` for incremental updates.
//!
//! The main loop uses `poll(2)` with a 1s timeout: timeouts trigger a stats
//! dump to compute throughput, readable mcast events trigger a full Snapshot.

use std::collections::HashMap;
use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::io::RawFd;
use std::process::Command;
use std::ptr;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperState {
    Up,
    Down,
    Unknown,
}

impl OperState {
    pub fn as_str(self) -> &'static str {
        match self {
            OperState::Up => "up",
            OperState::Down => "down",
            OperState::Unknown => "unknown",
        }
    }

    pub fn is_up(self) -> bool {
        matches!(self, OperState::Up)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IfaceSnapshot {
    pub index: u32,
    pub name: String,
    pub operstate: OperState,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Vec<Ipv4Addr>,
    pub ipv6: Vec<Ipv6Addr>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub is_wireless: bool,
    pub is_physical: bool,
    pub wifi_ssid: Option<String>,
}

#[derive(Debug, Clone)]
pub enum NetEvent {
    Snapshot(Vec<IfaceSnapshot>),
    Rates(HashMap<u32, (f64, f64)>), // (rx_bps, tx_bps)
}

// ───────────────────────────── netlink constants ─────────────────────────────

const NETLINK_ROUTE: i32 = 0;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;

const NLMSG_NOOP: u16 = 0x1;
const NLMSG_ERROR: u16 = 0x2;
const NLMSG_DONE: u16 = 0x3;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;

const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;

const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_ROOT: u16 = 0x100;
const NLM_F_MATCH: u16 = 0x200;
const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;

const RTNLGRP_LINK: u32 = 1;
const RTNLGRP_IPV4_IFADDR: u32 = 5;
const RTNLGRP_IPV6_IFADDR: u32 = 9;

// link attributes
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_STATS64: u16 = 23;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;

// addr attributes
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;

// operstate values (if.h)
const IF_OPER_DOWN: u8 = 2;
const IF_OPER_UP: u8 = 6;

// ───────────────────────────── wire structs ─────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Nlmsghdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Ifinfomsg {
    ifi_family: u8,
    _pad: u8,
    ifi_type: u16,
    ifi_index: i32,
    ifi_flags: u32,
    ifi_change: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Ifaddrmsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rtattr {
    rta_len: u16,
    rta_type: u16,
}

fn align4(x: usize) -> usize {
    (x + 3) & !3
}

const NLMSG_HDR_LEN: usize = mem::size_of::<Nlmsghdr>();
const IFINFO_LEN: usize = mem::size_of::<Ifinfomsg>();
const IFADDR_LEN: usize = mem::size_of::<Ifaddrmsg>();
const RTA_HDR_LEN: usize = mem::size_of::<Rtattr>();

// ───────────────────────────── socket helpers ─────────────────────────────

struct NlSock {
    fd: RawFd,
}

impl NlSock {
    fn open() -> io::Result<Self> {
        // SAFETY: plain libc syscall.
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_ROUTE) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // bind with pid=0 so kernel assigns one
        let mut sa: libc::sockaddr_nl = unsafe { mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as u16;
        let rc = unsafe {
            libc::bind(
                fd,
                &sa as *const _ as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(Self { fd })
    }

    fn add_membership(&self, group: u32) -> io::Result<()> {
        let g = group;
        let rc = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_NETLINK,
                NETLINK_ADD_MEMBERSHIP,
                &g as *const u32 as *const libc::c_void,
                mem::size_of::<u32>() as u32,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(self.fd, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let rc = unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn send_all(&self, buf: &[u8]) -> io::Result<()> {
        let mut sa: libc::sockaddr_nl = unsafe { mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as u16;
        let n = unsafe {
            libc::sendto(
                self.fd,
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                0,
                &sa as *const _ as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Receive next datagram into buf; returns number of bytes.
    /// Returns Ok(0) on WouldBlock for nonblocking sockets.
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }
}

impl Drop for NlSock {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

// ───────────────────────────── message builders ─────────────────────────────

fn build_get_request(msg_type: u16, family: u8, seq: u32) -> Vec<u8> {
    // header + ifinfomsg OR ifaddrmsg (both 16 bytes aligned fine) ... but we need
    // proper struct for each: for RTM_GETLINK use ifinfomsg; for RTM_GETADDR use ifaddrmsg.
    let payload_len = if msg_type == RTM_GETLINK {
        IFINFO_LEN
    } else {
        IFADDR_LEN
    };
    let total = NLMSG_HDR_LEN + payload_len;
    let mut buf = vec![0u8; total];

    let hdr = Nlmsghdr {
        nlmsg_len: total as u32,
        nlmsg_type: msg_type,
        nlmsg_flags: NLM_F_REQUEST | NLM_F_DUMP,
        nlmsg_seq: seq,
        nlmsg_pid: 0,
    };
    // SAFETY: buf has size_of::<Nlmsghdr>() + payload, we write header first.
    unsafe {
        ptr::write(buf.as_mut_ptr() as *mut Nlmsghdr, hdr);
    }
    if msg_type == RTM_GETLINK {
        let mut m: Ifinfomsg = unsafe { mem::zeroed() };
        m.ifi_family = family;
        unsafe {
            ptr::write(buf.as_mut_ptr().add(NLMSG_HDR_LEN) as *mut Ifinfomsg, m);
        }
    } else {
        let mut m: Ifaddrmsg = unsafe { mem::zeroed() };
        m.ifa_family = family;
        unsafe {
            ptr::write(buf.as_mut_ptr().add(NLMSG_HDR_LEN) as *mut Ifaddrmsg, m);
        }
    }
    buf
}

// ───────────────────────────── rtattr iterator ─────────────────────────────

struct RtaIter<'a> {
    buf: &'a [u8],
}

impl<'a> RtaIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl<'a> Iterator for RtaIter<'a> {
    type Item = (u16, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.len() < RTA_HDR_LEN {
            return None;
        }
        let hdr = unsafe { ptr::read_unaligned(self.buf.as_ptr() as *const Rtattr) };
        let rta_len = hdr.rta_len as usize;
        if rta_len < RTA_HDR_LEN || rta_len > self.buf.len() {
            return None;
        }
        let payload = &self.buf[RTA_HDR_LEN..rta_len];
        let next_off = align4(rta_len);
        self.buf = if next_off >= self.buf.len() {
            &[]
        } else {
            &self.buf[next_off..]
        };
        Some((hdr.rta_type, payload))
    }
}

// ───────────────────────────── parsers ─────────────────────────────

#[derive(Clone)]
struct LinkInfo {
    index: u32,
    name: String,
    operstate: OperState,
    mac: Option<[u8; 6]>,
    rx_bytes: u64,
    tx_bytes: u64,
    is_physical: bool,
}

fn parse_operstate(v: u8) -> OperState {
    match v {
        IF_OPER_UP => OperState::Up,
        IF_OPER_DOWN => OperState::Down,
        _ => OperState::Unknown,
    }
}

fn parse_link_msg(payload: &[u8]) -> Option<LinkInfo> {
    if payload.len() < IFINFO_LEN {
        return None;
    }
    let ifi = unsafe { ptr::read_unaligned(payload.as_ptr() as *const Ifinfomsg) };
    let attrs = &payload[IFINFO_LEN..];
    let mut info = LinkInfo {
        index: ifi.ifi_index as u32,
        name: String::new(),
        operstate: OperState::Unknown,
        mac: None,
        rx_bytes: 0,
        tx_bytes: 0,
        is_physical: true,
    };
    for (ty, data) in RtaIter::new(attrs) {
        match ty {
            IFLA_IFNAME => {
                let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                info.name = String::from_utf8_lossy(&data[..end]).to_string();
            }
            IFLA_ADDRESS => {
                if data.len() == 6 {
                    let mut m = [0u8; 6];
                    m.copy_from_slice(data);
                    info.mac = Some(m);
                }
            }
            IFLA_OPERSTATE => {
                if let Some(&v) = data.first() {
                    info.operstate = parse_operstate(v);
                }
            }
            IFLA_STATS64 => {
                // struct rtnl_link_stats64 { u64 rx_packets, tx_packets, rx_bytes, tx_bytes, ... }
                if data.len() >= 32 {
                    let rx_bytes = u64::from_ne_bytes(data[16..24].try_into().unwrap());
                    let tx_bytes = u64::from_ne_bytes(data[24..32].try_into().unwrap());
                    info.rx_bytes = rx_bytes;
                    info.tx_bytes = tx_bytes;
                }
            }
            IFLA_LINKINFO => {
                // IFLA_INFO_KIND present ⇒ virtual (veth/bridge/vlan/tun/...).
                if RtaIter::new(data).any(|(nty, _)| nty == IFLA_INFO_KIND) {
                    info.is_physical = false;
                }
            }
            _ => {}
        }
    }
    // Loopback has no IFLA_LINKINFO but is always virtual.
    if info.name == "lo" {
        info.is_physical = false;
    }
    Some(info)
}

#[derive(Clone)]
struct AddrInfo {
    index: u32,
    family: u8,
    ipv4: Option<Ipv4Addr>,
    ipv6: Option<Ipv6Addr>,
}

fn parse_addr_msg(payload: &[u8]) -> Option<AddrInfo> {
    if payload.len() < IFADDR_LEN {
        return None;
    }
    let ifa = unsafe { ptr::read_unaligned(payload.as_ptr() as *const Ifaddrmsg) };
    let attrs = &payload[IFADDR_LEN..];
    let mut info = AddrInfo {
        index: ifa.ifa_index,
        family: ifa.ifa_family,
        ipv4: None,
        ipv6: None,
    };
    // Prefer IFA_LOCAL (point-to-point), fall back to IFA_ADDRESS.
    let mut local_v4: Option<Ipv4Addr> = None;
    let mut remote_v4: Option<Ipv4Addr> = None;
    let mut local_v6: Option<Ipv6Addr> = None;
    let mut remote_v6: Option<Ipv6Addr> = None;
    for (ty, data) in RtaIter::new(attrs) {
        match (ty, ifa.ifa_family as i32) {
            (IFA_ADDRESS, libc::AF_INET) if data.len() == 4 => {
                remote_v4 = Some(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
            }
            (IFA_LOCAL, libc::AF_INET) if data.len() == 4 => {
                local_v4 = Some(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
            }
            (IFA_ADDRESS, libc::AF_INET6) if data.len() == 16 => {
                let mut b = [0u8; 16];
                b.copy_from_slice(data);
                remote_v6 = Some(Ipv6Addr::from(b));
            }
            (IFA_LOCAL, libc::AF_INET6) if data.len() == 16 => {
                let mut b = [0u8; 16];
                b.copy_from_slice(data);
                local_v6 = Some(Ipv6Addr::from(b));
            }
            _ => {}
        }
    }
    info.ipv4 = local_v4.or(remote_v4);
    info.ipv6 = local_v6.or(remote_v6);
    Some(info)
}

// ───────────────────────────── nlmsg iteration ─────────────────────────────

/// Iterate over possibly-multipart netlink messages in `buf`. Each yielded
/// tuple is (msg_type, flags, payload).
struct NlmsgIter<'a> {
    buf: &'a [u8],
}

impl<'a> Iterator for NlmsgIter<'a> {
    type Item = (u16, u16, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.len() < NLMSG_HDR_LEN {
            return None;
        }
        let hdr = unsafe { ptr::read_unaligned(self.buf.as_ptr() as *const Nlmsghdr) };
        let len = hdr.nlmsg_len as usize;
        if len < NLMSG_HDR_LEN || len > self.buf.len() {
            return None;
        }
        let payload = &self.buf[NLMSG_HDR_LEN..len];
        let next_off = align4(len);
        self.buf = if next_off >= self.buf.len() {
            &[]
        } else {
            &self.buf[next_off..]
        };
        Some((hdr.nlmsg_type, hdr.nlmsg_flags, payload))
    }
}

// ───────────────────────────── dump helpers ─────────────────────────────

/// Receive all multipart responses for a dump request until NLMSG_DONE.
/// Returns collected (type, payload) items.
fn recv_dump(sock: &NlSock) -> io::Result<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        // blocking recv
        let n = unsafe { libc::recv(sock.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let slice = &buf[..n as usize];
        let mut done = false;
        for (ty, _flags, payload) in (NlmsgIter { buf: slice }) {
            match ty {
                NLMSG_DONE => {
                    done = true;
                }
                NLMSG_ERROR => {
                    // first 4 bytes is errno (i32)
                    if payload.len() >= 4 {
                        let err = i32::from_ne_bytes(payload[..4].try_into().unwrap());
                        if err != 0 {
                            return Err(io::Error::from_raw_os_error(-err));
                        }
                    }
                    done = true;
                }
                NLMSG_NOOP => {}
                _ => {
                    out.push((ty, payload.to_vec()));
                }
            }
        }
        if done {
            break;
        }
    }
    Ok(out)
}

fn dump_links(sock: &NlSock, seq: &mut u32) -> io::Result<Vec<LinkInfo>> {
    *seq = seq.wrapping_add(1);
    let req = build_get_request(RTM_GETLINK, libc::AF_UNSPEC as u8, *seq);
    sock.send_all(&req)?;
    let msgs = recv_dump(sock)?;
    let mut out = Vec::new();
    for (ty, payload) in msgs {
        if ty == RTM_NEWLINK {
            if let Some(l) = parse_link_msg(&payload) {
                out.push(l);
            }
        }
    }
    Ok(out)
}

fn dump_addrs(sock: &NlSock, seq: &mut u32, family: u8) -> io::Result<Vec<AddrInfo>> {
    *seq = seq.wrapping_add(1);
    let req = build_get_request(RTM_GETADDR, family, *seq);
    sock.send_all(&req)?;
    let msgs = recv_dump(sock)?;
    let mut out = Vec::new();
    for (ty, payload) in msgs {
        if ty == RTM_NEWADDR {
            if let Some(a) = parse_addr_msg(&payload) {
                out.push(a);
            }
        }
    }
    Ok(out)
}

// ───────────────────────────── iwgetid ─────────────────────────────

pub fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Compact rate like "1.2M" / "340K" / "0B" — used on the status bar.
pub fn format_rate_short(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bps >= MB {
        format!("{:.1}M", bps / MB)
    } else if bps >= KB {
        format!("{:.0}K", bps / KB)
    } else {
        format!("{:.0}B", bps)
    }
}

/// Rate with unit like "1.23 MB/s" — used in the popup detail panel.
pub fn format_rate_long(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if bps >= GB {
        format!("{:.2} GB/s", bps / GB)
    } else if bps >= MB {
        format!("{:.2} MB/s", bps / MB)
    } else if bps >= KB {
        format!("{:.1} KB/s", bps / KB)
    } else {
        format!("{:.0} B/s", bps)
    }
}

pub fn format_bytes(b: u64) -> String {
    let bf = b as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    if bf >= TB {
        format!("{:.2} TB", bf / TB)
    } else if bf >= GB {
        format!("{:.2} GB", bf / GB)
    } else if bf >= MB {
        format!("{:.2} MB", bf / MB)
    } else if bf >= KB {
        format!("{:.1} KB", bf / KB)
    } else {
        format!("{b} B")
    }
}

fn iwgetid_ssid() -> Option<String> {
    let output = Command::new("iwgetid").arg("-r").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let ssid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if ssid.is_empty() {
        None
    } else {
        Some(ssid)
    }
}

// ───────────────────────────── snapshot merging ─────────────────────────────

fn merge_snapshot(
    links: &[LinkInfo],
    addrs: &[AddrInfo],
    wifi_cache: &HashMap<u32, Option<String>>,
) -> Vec<IfaceSnapshot> {
    let mut out: Vec<IfaceSnapshot> = Vec::new();
    for l in links {
        if l.name == "lo" {
            continue;
        }
        let is_wireless = l.name.starts_with("wl");
        let mut snap = IfaceSnapshot {
            index: l.index,
            name: l.name.clone(),
            operstate: l.operstate,
            mac: l.mac,
            ipv4: vec![],
            ipv6: vec![],
            rx_bytes: l.rx_bytes,
            tx_bytes: l.tx_bytes,
            is_wireless,
            is_physical: l.is_physical,
            wifi_ssid: wifi_cache.get(&l.index).cloned().flatten(),
        };
        for a in addrs {
            if a.index != l.index {
                continue;
            }
            if a.family as i32 == libc::AF_INET {
                if let Some(v) = a.ipv4 {
                    if !snap.ipv4.contains(&v) {
                        snap.ipv4.push(v);
                    }
                }
            } else if a.family as i32 == libc::AF_INET6 {
                if let Some(v) = a.ipv6 {
                    if !snap.ipv6.contains(&v) {
                        snap.ipv6.push(v);
                    }
                }
            }
        }
        out.push(snap);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ───────────────────────────── worker entry ─────────────────────────────

/// Spawn the netlink worker thread. The thread terminates when the receiver
/// end of `tx` is dropped.
pub fn spawn_netlink_worker(tx: async_channel::Sender<NetEvent>) {
    std::thread::spawn(move || {
        if let Err(e) = run_worker(tx) {
            tracing::warn!("netlink worker exited: {e}");
        }
    });
}

fn run_worker(tx: async_channel::Sender<NetEvent>) -> io::Result<()> {
    let dump_sock = NlSock::open()?;
    let mcast_sock = NlSock::open()?;
    mcast_sock.add_membership(RTNLGRP_LINK)?;
    mcast_sock.add_membership(RTNLGRP_IPV4_IFADDR)?;
    mcast_sock.add_membership(RTNLGRP_IPV6_IFADDR)?;
    mcast_sock.set_nonblocking()?;

    let mut seq: u32 = 0;

    // Initial full dump.
    let links = dump_links(&dump_sock, &mut seq)?;
    let v4 = dump_addrs(&dump_sock, &mut seq, libc::AF_INET as u8)?;
    let v6 = dump_addrs(&dump_sock, &mut seq, libc::AF_INET6 as u8)?;
    let mut addrs: Vec<AddrInfo> = v4;
    addrs.extend(v6);

    let mut wifi_cache: HashMap<u32, Option<String>> = HashMap::new();
    refresh_wifi_cache(&links, &mut wifi_cache);

    let mut snapshot = merge_snapshot(&links, &addrs, &wifi_cache);
    if tx
        .send_blocking(NetEvent::Snapshot(snapshot.clone()))
        .is_err()
    {
        return Ok(());
    }

    // Cache structures for incremental updates.
    let mut link_cache: HashMap<u32, LinkInfo> =
        links.iter().map(|l| (l.index, l.clone())).collect();
    let mut addr_cache: Vec<AddrInfo> = addrs;

    // rate calc state
    let mut last_stats: HashMap<u32, (u64, u64, Instant)> = HashMap::new();
    for l in link_cache.values() {
        last_stats.insert(l.index, (l.rx_bytes, l.tx_bytes, Instant::now()));
    }

    let mut mcast_buf = vec![0u8; 32 * 1024];
    loop {
        // poll mcast fd with 1s timeout
        let mut pfd = libc::pollfd {
            fd: mcast_sock.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 1000) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        if rc == 0 {
            // timeout → rates
            match dump_links(&dump_sock, &mut seq) {
                Ok(new_links) => {
                    let now = Instant::now();
                    let mut rates: HashMap<u32, (f64, f64)> = HashMap::new();
                    for l in &new_links {
                        if l.name == "lo" {
                            continue;
                        }
                        if let Some(&(prev_rx, prev_tx, prev_ts)) = last_stats.get(&l.index) {
                            let dt = now.duration_since(prev_ts).as_secs_f64();
                            if dt > 0.0 {
                                let rx_d = l.rx_bytes.saturating_sub(prev_rx) as f64;
                                let tx_d = l.tx_bytes.saturating_sub(prev_tx) as f64;
                                rates.insert(l.index, (rx_d / dt, tx_d / dt));
                            }
                        }
                        last_stats.insert(l.index, (l.rx_bytes, l.tx_bytes, now));
                    }
                    // also refresh totals in cache
                    for l in &new_links {
                        if let Some(existing) = link_cache.get_mut(&l.index) {
                            existing.rx_bytes = l.rx_bytes;
                            existing.tx_bytes = l.tx_bytes;
                        }
                    }
                    // refresh snapshot totals so the UI shows current totals
                    let refreshed = merge_snapshot(
                        &link_cache.values().cloned().collect::<Vec<_>>(),
                        &addr_cache,
                        &wifi_cache,
                    );
                    if refreshed != snapshot {
                        snapshot = refreshed.clone();
                        if tx.send_blocking(NetEvent::Snapshot(refreshed)).is_err() {
                            return Ok(());
                        }
                    }
                    if tx.send_blocking(NetEvent::Rates(rates)).is_err() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    tracing::debug!("netlink stats dump failed: {e}");
                }
            }
            continue;
        }

        // readable → drain mcast socket
        let mut link_changed = false;
        loop {
            let n = match mcast_sock.recv(&mut mcast_buf) {
                Ok(0) => break, // would-block
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!("mcast recv err: {e}");
                    break;
                }
            };
            let slice = &mcast_buf[..n];
            for (ty, _flags, payload) in (NlmsgIter { buf: slice }) {
                match ty {
                    RTM_NEWLINK => {
                        if let Some(l) = parse_link_msg(payload) {
                            link_cache.insert(l.index, l);
                            link_changed = true;
                        }
                    }
                    RTM_DELLINK => {
                        if let Some(l) = parse_link_msg(payload) {
                            link_cache.remove(&l.index);
                            last_stats.remove(&l.index);
                            link_changed = true;
                        }
                    }
                    RTM_NEWADDR => {
                        if let Some(a) = parse_addr_msg(payload) {
                            // replace any existing entry for same (index, family, address)
                            addr_cache.retain(|x| {
                                !(x.index == a.index
                                    && x.family == a.family
                                    && x.ipv4 == a.ipv4
                                    && x.ipv6 == a.ipv6)
                            });
                            addr_cache.push(a);
                        }
                    }
                    RTM_DELADDR => {
                        if let Some(a) = parse_addr_msg(payload) {
                            addr_cache.retain(|x| {
                                !(x.index == a.index
                                    && x.family == a.family
                                    && x.ipv4 == a.ipv4
                                    && x.ipv6 == a.ipv6)
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        if link_changed {
            let links_vec: Vec<LinkInfo> = link_cache.values().cloned().collect();
            refresh_wifi_cache(&links_vec, &mut wifi_cache);
        }

        let refreshed = merge_snapshot(
            &link_cache.values().cloned().collect::<Vec<_>>(),
            &addr_cache,
            &wifi_cache,
        );
        if refreshed != snapshot {
            snapshot = refreshed.clone();
            if tx.send_blocking(NetEvent::Snapshot(refreshed)).is_err() {
                return Ok(());
            }
        }
    }
}

fn refresh_wifi_cache(links: &[LinkInfo], cache: &mut HashMap<u32, Option<String>>) {
    // Only issue iwgetid once per refresh; it reports the currently associated
    // SSID regardless of interface, so we apply it to whichever wl* is up.
    let mut ssid: Option<String> = None;
    let mut queried = false;
    for l in links {
        if !l.name.starts_with("wl") {
            cache.remove(&l.index);
            continue;
        }
        if !queried {
            ssid = iwgetid_ssid();
            queried = true;
        }
        if l.operstate.is_up() {
            cache.insert(l.index, ssid.clone());
        } else {
            cache.insert(l.index, None);
        }
    }
    // prune stale entries
    cache.retain(|idx, _| links.iter().any(|l| l.index == *idx));
}
