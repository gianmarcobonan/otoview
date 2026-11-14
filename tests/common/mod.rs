//! End-to-end over loopback: client <-> simulator, both firmware families.

use otoview::protocol::{Config, Device, Event, Family};
use otoview::sim;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub fn roundtrip(family: Family) {
    let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
    std::thread::spawn(move || sim::run(family, lo).unwrap());
    std::thread::sleep(Duration::from_millis(200));

    let (tx, rx) = mpsc::channel();
    let dev = Device::start(Config { host: lo, tx_video_local_port: 0, ..Default::default() }, tx).unwrap();
    dev.open();

    let (mut info, mut battery, mut frame) = (false, false, false);
    let end = Instant::now() + Duration::from_secs(5);
    while !(info && battery && frame) && Instant::now() < end {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Info(f, _)) => info = f == family,
            Ok(Event::Battery(_)) => battery = true,
            Ok(Event::Frame { jpeg, .. }) => {
                assert_eq!((&jpeg[..2], &jpeg[jpeg.len() - 2..]), (&[0xFF, 0xD8][..], &[0xFF, 0xD9][..]));
                frame = true;
            }
            _ => {}
        }
    }
    assert!(info && battery && frame, "info={info} battery={battery} frame={frame}");
    assert_eq!(dev.family(), Some(family));
}

