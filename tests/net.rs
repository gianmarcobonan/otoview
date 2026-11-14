//! Interface selection (home router also at 192.168.1.1) and clean restart.
use otoview::protocol::{Config, Device, Family, NetIface, find_device_iface};
use otoview::sim;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn finder_picks_the_interface_where_the_device_answers() {
    let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
    std::thread::spawn(move || sim::run(Family::Tx, lo).unwrap());
    std::thread::sleep(Duration::from_millis(200));

    // A real LAN interface (where nothing answers on the device ports) plus loopback.
    let mut cands: Vec<NetIface> = if_addrs::get_if_addrs()
        .unwrap()
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) if !i.is_loopback() => Some(NetIface { name: i.name, ip: a.ip, index: None }),
            _ => None,
        })
        .take(1)
        .collect();
    let loopback = NetIface { name: "loopback".into(), ip: Ipv4Addr::LOCALHOST, index: None };
    cands.push(loopback.clone());
    assert_eq!(find_device_iface(lo, &cands, Duration::from_secs(2)), Some(loopback));
}

#[test]
fn stop_releases_the_fixed_port() {
    let cfg = Config { host: IpAddr::V4(Ipv4Addr::LOCALHOST), tx_video_local_port: 61599, iface: None };
    for _ in 0..3 {
        let (tx, _rx) = mpsc::channel();
        let dev = Device::start(cfg.clone(), tx).expect("port still busy after stop()");
        dev.stop();
    }
}
