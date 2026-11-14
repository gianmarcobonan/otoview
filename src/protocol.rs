//! Wi-Fi endoscope UDP protocol, both firmware families. See PROTOCOL.md.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEVICE_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
pub const TX_CTRL_PORT: u16 = 61500;
pub const TX_VIDEO_PORT: u16 = 61501;
pub const TEXT_CTRL_PORT: u16 = 61502;
pub const TEXT_VIDEO_PORT: u16 = 61503;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Query-string control (`type=1002\n`), ports 61502/61503.
    Text,
    /// Binary commands with JSON replies, ports 61500/61501.
    Tx,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub ssid: String,
    pub firmware: i64,
    /// Only meaningful for `Family::Text`; ≥2 changes the open command.
    pub protocol: u16,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Info(Family, Info),
    /// 0..=100 percent, 101 = charging.
    Battery(u8),
    /// Complete JPEG plus the device's G-sensor angle for that frame.
    Frame { jpeg: Vec<u8>, angle: f32 },
    /// Video arrived within the last 2 s.
    Connected(bool),
}

// ---------- commands ----------

pub fn cmd_info(f: Family) -> Vec<u8> {
    match f {
        Family::Text => b"type=1002\n".to_vec(),
        Family::Tx => vec![0x10, 0x01],
    }
}

pub fn cmd_battery(f: Family) -> Vec<u8> {
    match f {
        Family::Text => b"type=1001\n".to_vec(),
        Family::Tx => vec![0x10, 0x01], // same request; the JSON reply carries the battery
    }
}

pub fn cmd_led(f: Family, level: u8) -> Vec<u8> {
    let level = level.min(100);
    match f {
        Family::Text => format!("type=1003&value={level}\n").into_bytes(),
        Family::Tx => vec![0x10, 0x02, 0x00, level],
    }
}

pub fn cmd_open(f: Family, protocol: u16) -> Vec<u8> {
    match f {
        Family::Text if protocol >= 2 => {
            let p = protocol.to_be_bytes();
            vec![0x20, 0x36, p[0], p[1]]
        }
        Family::Text => vec![0x20, 0x36],
        Family::Tx => vec![0x20, 0x01],
    }
}

pub fn cmd_close(f: Family) -> Vec<u8> {
    match f {
        Family::Text => vec![0x20, 0x37],
        Family::Tx => vec![0x20, 0x02],
    }
}

// ---------- control replies ----------

pub fn parse_text_ctrl(data: &[u8]) -> Vec<Event> {
    // ponytail: "LOGO" chunks (device branding image) are ignored.
    if data.starts_with(b"LOGO") {
        return vec![];
    }
    // ponytail: no percent-decoding of values; add it if an SSID ever shows %xx.
    let s = String::from_utf8_lossy(data).replace('\n', "");
    let kv: HashMap<&str, &str> = s.split('&').filter_map(|p| p.split_once('=')).collect();
    let num = |k: &str| kv.get(k).and_then(|v| v.trim().parse::<i64>().ok());
    match kv.get("type").copied() {
        Some("2001") => num("data")
            .map(|b| vec![Event::Battery(b.clamp(0, 101) as u8)])
            .unwrap_or_default(),
        Some("2002") => vec![Event::Info(
            Family::Text,
            Info {
                width: num("w").unwrap_or(0) as u32,
                height: num("h").unwrap_or(0) as u32,
                fps: num("fps").unwrap_or(0) as u32,
                ssid: kv.get("ssid").unwrap_or(&"").to_string(),
                firmware: num("firmware").unwrap_or(0),
                protocol: num("protocol").unwrap_or(1) as u16,
            },
        )],
        _ => vec![],
    }
}

pub fn parse_tx_ctrl(data: &[u8]) -> Vec<Event> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else {
        return vec![];
    };
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let info = Info {
        width: n("width") as u32,
        height: n("height") as u32,
        fps: n("fps") as u32,
        ssid: v.get("ssid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        firmware: n("firmware_version"),
        protocol: 1,
    };
    let charging = v.get("battery_charging").and_then(|x| x.as_bool()) == Some(true);
    let battery = if charging { 101 } else { n("battery_percentage").clamp(0, 100) as u8 };
    vec![Event::Info(Family::Tx, info), Event::Battery(battery)]
}

// ---------- video reassembly ----------

/// First FFD8 .. last FFD9, as the original app does.
pub fn extract_jpeg(buf: &[u8]) -> Option<Vec<u8>> {
    let start = buf.windows(2).position(|w| w == [0xFF, 0xD8])?;
    let end = buf.windows(2).rposition(|w| w == [0xFF, 0xD9])?;
    (end > start).then(|| buf[start..end + 2].to_vec())
}

/// Text family: `[frame_id, is_last, ?, frag_idx, payload...]`, payload = `pkt[4..]`;
/// the first fragment's payload starts with an int16 BE angle.
#[derive(Default)]
pub struct TextAssembler {
    frags: Vec<(Instant, Vec<u8>)>,
}

impl TextAssembler {
    pub fn push(&mut self, pkt: &[u8], now: Instant) -> Option<(Vec<u8>, f32)> {
        if pkt.len() <= 8 {
            return None;
        }
        self.frags.retain(|(t, _)| now.duration_since(*t) <= Duration::from_millis(200));
        self.frags.push((now, pkt.to_vec()));
        // Inferred: byte 1 == 1 marks the last fragment and triggers assembly.
        if pkt[1] != 1 {
            return None;
        }
        let id = pkt[0];
        let (mut mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.frags).into_iter().partition(|(_, p)| p[0] == id);
        self.frags = rest;
        mine.sort_by_key(|(_, p)| p[3]);
        let first = &mine[0].1;
        let angle = i16::from_be_bytes([first[4], first[5]]) as f32;
        let buf: Vec<u8> = mine.iter().flat_map(|(_, p)| p[4..].iter().copied()).collect();
        extract_jpeg(&buf).map(|j| (j, angle))
    }
}

/// TX family: `[frame_id, frag_idx, frag_count, angle f32 BE (4), ?(5), payload...]`,
/// payload = `pkt[12..]`.
#[derive(Default)]
pub struct TxAssembler {
    frags: Vec<Vec<u8>>,
}

impl TxAssembler {
    pub fn push(&mut self, pkt: &[u8]) -> Option<(Vec<u8>, f32)> {
        if pkt.len() <= 12 {
            return None;
        }
        if pkt[1] == 0 {
            self.frags.clear();
        }
        self.frags.push(pkt.to_vec());
        let (id, count) = (pkt[0], pkt[2] as usize);
        if self.frags.len() != count {
            return None;
        }
        let mut buf = Vec::new();
        for i in 0..count {
            let p = self.frags.iter().find(|p| p[0] == id && p[1] as usize == i)?;
            buf.extend_from_slice(&p[12..]);
        }
        let angle = f32::from_be_bytes([pkt[3], pkt[4], pkt[5], pkt[6]]);
        extract_jpeg(&buf).map(|j| (j, angle))
    }
}

// ---------- network interfaces ----------

/// A local IPv4 interface. Used when several interfaces share the device's subnet
/// (home routers are often 192.168.1.1 too): traffic must leave via the endoscope Wi-Fi.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetIface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub index: Option<u32>,
}

/// True if `ip` is on the same subnet as `host` (and is not `host` itself).
pub fn on_subnet(ip: Ipv4Addr, mask: Ipv4Addr, host: Ipv4Addr) -> bool {
    let (ip, mask, host) = (u32::from(ip), u32::from(mask), u32::from(host));
    ip != host && ip & mask == host & mask
}

/// Interfaces that could reach `host` directly. More than one = possible conflict.
pub fn candidate_ifaces(host: Ipv4Addr) -> Vec<NetIface> {
    let all = if_addrs::get_if_addrs().unwrap_or_default();
    all.into_iter()
        .filter(|i| !i.is_loopback() && i.is_oper_up())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) if on_subnet(a.ip, a.netmask, host) => {
                Some(NetIface { name: i.name, ip: a.ip, index: i.index })
            }
            _ => None,
        })
        .collect()
}

/// UDP socket on `port` (0 = any), optionally pinned to one interface.
/// Windows sends from the interface owning the bound IP (strong host model);
/// macOS/Linux also need the interface index set on the socket.
fn udp(iface: Option<&NetIface>, port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    #[cfg(not(windows))]
    if let Some(idx) = iface.and_then(|i| i.index).and_then(std::num::NonZeroU32::new) {
        // ponytail: best effort; where this is refused (old Linux kernels without
        // privileges) the routing table decides.
        let _ = s.bind_device_by_index_v4(Some(idx));
    }
    let ip = iface.map_or(Ipv4Addr::UNSPECIFIED, |i| i.ip);
    s.bind(&std::net::SocketAddr::from((ip, port)).into())?;
    s.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(s.into())
}

/// Binds `base + 0..499` like the original app; falls back to any port.
fn bind_near(iface: Option<&NetIface>, base: u16) -> io::Result<UdpSocket> {
    if base == 0 {
        return udp(iface, 0);
    }
    let jitter = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() % 500;
    udp(iface, base + jitter as u16).or_else(|_| udp(iface, 0))
}

/// Sends both info probes from each candidate and returns the first interface on
/// which `host` answers like an endoscope.
pub fn find_device_iface(host: IpAddr, candidates: &[NetIface], timeout: Duration) -> Option<NetIface> {
    let socks: Vec<(usize, Family, UdpSocket)> = candidates
        .iter()
        .enumerate()
        .flat_map(|(i, c)| [(i, c, Family::Text, TEXT_CTRL_PORT), (i, c, Family::Tx, TX_CTRL_PORT)])
        .filter_map(|(i, c, f, port)| {
            let s = udp(Some(c), 0).ok()?;
            s.set_read_timeout(Some(Duration::from_millis(20))).ok()?;
            s.send_to(&cmd_info(f), (host, port)).ok()?;
            Some((i, f, s))
        })
        .collect();
    let end = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        for (i, f, s) in &socks {
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                let parsed = match f {
                    Family::Text => parse_text_ctrl(&buf[..n]),
                    Family::Tx => parse_tx_ctrl(&buf[..n]),
                };
                if from.ip() == host && !parsed.is_empty() {
                    return Some(candidates[*i].clone());
                }
            }
        }
    }
    None
}

// ---------- client ----------

#[derive(Clone, Debug)]
pub struct Config {
    pub host: IpAddr,
    /// The TX firmware streams to this fixed local port. 0 = any (tests/simulator).
    pub tx_video_local_port: u16,
    /// None = let the routing table choose.
    pub iface: Option<NetIface>,
}

impl Default for Config {
    fn default() -> Self {
        Config { host: IpAddr::V4(DEVICE_IP), tx_video_local_port: TX_VIDEO_PORT, iface: None }
    }
}

#[derive(Default)]
struct State {
    family: Option<Family>,
    protocol: u16,
    last_ctrl: Option<Instant>,
    last_video: Option<Instant>,
    streaming: bool,
}

pub struct Device {
    host: IpAddr,
    pub iface: Option<NetIface>,
    text_ctrl: UdpSocket,
    text_video: UdpSocket,
    tx_ctrl: UdpSocket,
    tx_video: UdpSocket,
    state: Mutex<State>,
    alive: AtomicBool,
    threads: Mutex<Vec<thread::JoinHandle<()>>>,
}

fn fresh(t: Option<Instant>) -> bool {
    t.is_some_and(|t| t.elapsed() < Duration::from_secs(2))
}

impl Device {
    /// Opens the four sockets and starts the reader and timer threads (until `stop`).
    pub fn start(cfg: Config, events: Sender<Event>) -> io::Result<Arc<Device>> {
        let ifc = cfg.iface.as_ref();
        let tx_video = if cfg.tx_video_local_port == 0 { bind_near(ifc, 0)? } else { udp(ifc, cfg.tx_video_local_port)? };
        let dev = Arc::new(Device {
            host: cfg.host,
            text_ctrl: bind_near(ifc, 50000)?,
            text_video: bind_near(ifc, 51000)?,
            tx_ctrl: bind_near(ifc, 40000)?,
            tx_video,
            iface: cfg.iface,
            state: Mutex::new(State { protocol: 1, ..Default::default() }),
            alive: AtomicBool::new(true),
            threads: Mutex::default(),
        });

        dev.reader(events.clone(), |d| &d.text_ctrl, |d, pkt, ev| d.on_ctrl(Family::Text, pkt, ev));
        dev.reader(events.clone(), |d| &d.tx_ctrl, |d, pkt, ev| d.on_ctrl(Family::Tx, pkt, ev));
        let mut text_asm = TextAssembler::default();
        dev.reader(events.clone(), |d| &d.text_video, move |d, pkt, ev| {
            d.on_frame(text_asm.push(pkt, Instant::now()), ev)
        });
        let mut tx_asm = TxAssembler::default();
        dev.reader(events.clone(), |d| &d.tx_video, move |d, pkt, ev| d.on_frame(tx_asm.push(pkt), ev));

        let d = dev.clone();
        dev.threads.lock().unwrap().push(thread::spawn(move || d.timer(events)));
        Ok(dev)
    }

    /// Stops video and all threads; the sockets close when the last `Arc` is dropped.
    pub fn stop(&self) {
        self.close();
        self.alive.store(false, Ordering::Relaxed);
        let handles = std::mem::take(&mut *self.threads.lock().unwrap());
        for h in handles {
            let _ = h.join();
        }
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn connected(&self) -> bool {
        fresh(self.state.lock().unwrap().last_video)
    }

    pub fn family(&self) -> Option<Family> {
        self.state.lock().unwrap().family
    }

    /// Starts video now, or as soon as the device answers the info probe.
    pub fn open(&self) {
        let mut s = self.state.lock().unwrap();
        s.streaming = true;
        if let Some(f) = s.family {
            self.send_video(f, &cmd_open(f, s.protocol));
        }
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.streaming = false;
        if let Some(f) = s.family {
            self.send_video(f, &cmd_close(f));
        }
    }

    pub fn set_led(&self, level: u8) {
        if let Some(f) = self.family() {
            self.send_ctrl(f, &cmd_led(f, level));
        }
    }

    fn send_ctrl(&self, f: Family, data: &[u8]) {
        // Errors (no route while off the device Wi-Fi) are expected; the timer retries.
        let _ = match f {
            Family::Text => self.text_ctrl.send_to(data, (self.host, TEXT_CTRL_PORT)),
            Family::Tx => self.tx_ctrl.send_to(data, (self.host, TX_CTRL_PORT)),
        };
    }

    fn send_video(&self, f: Family, data: &[u8]) {
        let _ = match f {
            Family::Text => self.text_video.send_to(data, (self.host, TEXT_VIDEO_PORT)),
            Family::Tx => self.tx_video.send_to(data, (self.host, TX_VIDEO_PORT)),
        };
    }

    fn reader<S, H>(self: &Arc<Self>, events: Sender<Event>, sock: S, mut handle: H)
    where
        S: Fn(&Device) -> &UdpSocket + Send + 'static,
        H: FnMut(&Device, &[u8], &Sender<Event>) + Send + 'static,
    {
        let d = self.clone();
        let h = thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while d.alive() {
                match sock(&d).recv_from(&mut buf) {
                    Ok((n, from)) if from.ip() == d.host => handle(&d, &buf[..n], &events),
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    // Windows reports ICMP port-unreachable as a recv error: keep going.
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        self.threads.lock().unwrap().push(h);
    }

    fn on_ctrl(&self, f: Family, pkt: &[u8], events: &Sender<Event>) {
        let parsed = match f {
            Family::Text => parse_text_ctrl(pkt),
            Family::Tx => parse_tx_ctrl(pkt),
        };
        if parsed.is_empty() {
            return;
        }
        {
            let mut s = self.state.lock().unwrap();
            s.last_ctrl = Some(Instant::now());
            for e in &parsed {
                if let Event::Info(fam, info) = e {
                    let first = s.family.is_none();
                    s.family = Some(*fam);
                    s.protocol = info.protocol;
                    if first && s.streaming {
                        self.send_video(*fam, &cmd_open(*fam, s.protocol));
                    }
                }
            }
        }
        for e in parsed {
            let _ = events.send(e);
        }
    }

    fn on_frame(&self, frame: Option<(Vec<u8>, f32)>, events: &Sender<Event>) {
        if let Some((jpeg, angle)) = frame {
            self.state.lock().unwrap().last_video = Some(Instant::now());
            let _ = events.send(Event::Frame { jpeg, angle });
        }
    }

    /// 100 ms: probe both families until control replies; 1 s: battery; 500 ms: watchdog.
    fn timer(&self, events: Sender<Event>) {
        let mut connected = false;
        let mut tick = 0u64;
        while self.alive() {
            tick += 1;
            let (family, ctrl_ok, video_ok) = {
                let s = self.state.lock().unwrap();
                (s.family, fresh(s.last_ctrl), fresh(s.last_video))
            };
            match family {
                Some(f) if ctrl_ok => {
                    if tick.is_multiple_of(10) {
                        self.send_ctrl(f, &cmd_battery(f));
                    }
                }
                _ => {
                    self.send_ctrl(Family::Text, &cmd_info(Family::Text));
                    self.send_ctrl(Family::Tx, &cmd_info(Family::Tx));
                }
            }
            if tick.is_multiple_of(5) && video_ok != connected {
                connected = video_ok;
                if events.send(Event::Connected(connected)).is_err() {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(cmd_open(Family::Text, 1), [0x20, 0x36]);
        assert_eq!(cmd_open(Family::Text, 2), [0x20, 0x36, 0x00, 0x02]);
        assert_eq!(cmd_close(Family::Tx), [0x20, 0x02]);
        assert_eq!(cmd_led(Family::Text, 150), b"type=1003&value=100\n");
        assert_eq!(cmd_led(Family::Tx, 42), [0x10, 0x02, 0x00, 42]);
    }

    #[test]
    fn text_ctrl() {
        assert_eq!(parse_text_ctrl(b"type=2001&data=77\n"), vec![Event::Battery(77)]);
        let ev = parse_text_ctrl(b"type=2002&w=640&h=480&fps=25&ssid=softish-1&firmware=3&protocol=2");
        let Event::Info(Family::Text, i) = &ev[0] else { panic!("{ev:?}") };
        assert_eq!((i.width, i.height, i.fps, i.protocol, i.ssid.as_str()), (640, 480, 25, 2, "softish-1"));
        assert!(parse_text_ctrl(b"LOGO\x00\x01").is_empty());
    }

    #[test]
    fn tx_ctrl() {
        let ev = parse_tx_ctrl(br#"{"ssid":"x","width":1280,"height":720,"fps":30,"battery_charging":true,"battery_percentage":40}"#);
        assert_eq!(ev[1], Event::Battery(101));
        assert!(parse_tx_ctrl(b"garbage").is_empty());
    }

    #[test]
    fn subnet() {
        let m = Ipv4Addr::new(255, 255, 255, 0);
        assert!(on_subnet(Ipv4Addr::new(192, 168, 1, 23), m, DEVICE_IP));
        assert!(!on_subnet(Ipv4Addr::new(192, 168, 0, 23), m, DEVICE_IP));
        assert!(!on_subnet(DEVICE_IP, m, DEVICE_IP));
    }

    fn fake_jpeg(n: usize) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend((0..n).map(|i| (i % 200) as u8));
        j.extend([0xFF, 0xD9]);
        j
    }

    #[test]
    fn text_reassembly_out_of_order() {
        let jpeg = fake_jpeg(3000);
        let mut payload = (-15i16).to_be_bytes().to_vec();
        payload.extend(&jpeg);
        let chunks: Vec<_> = payload.chunks(1000).collect();
        let pkts: Vec<Vec<u8>> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| [&[7, (i == chunks.len() - 1) as u8, 0, i as u8][..], c].concat())
            .collect();
        let mut a = TextAssembler::default();
        let now = Instant::now();
        assert!(a.push(&pkts[1], now).is_none());
        assert!(a.push(&pkts[0], now).is_none());
        assert!(a.push(&pkts[2], now).is_none());
        assert_eq!(a.push(&pkts[3], now), Some((jpeg, -15.0)));
    }

    #[test]
    fn tx_reassembly() {
        let jpeg = fake_jpeg(2500);
        let chunks: Vec<_> = jpeg.chunks(1000).collect();
        let n = chunks.len() as u8;
        let mut a = TxAssembler::default();
        let mut out = None;
        for (i, c) in chunks.iter().enumerate() {
            let mut p = vec![3, i as u8, n];
            p.extend(90.5f32.to_be_bytes());
            p.extend([0; 5]);
            p.extend(*c);
            out = a.push(&p);
        }
        assert_eq!(out, Some((jpeg, 90.5)));
    }
}
