//! Fake endoscope for testing without hardware. Speaks one firmware family.

use crate::protocol::*;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CHUNK: usize = 1400;

/// Blocks forever serving `family` on `ip` (e.g. 127.0.0.1).
pub fn run(family: Family, ip: IpAddr) -> io::Result<()> {
    let (cp, vp) = match family {
        Family::Text => (TEXT_CTRL_PORT, TEXT_VIDEO_PORT),
        Family::Tx => (TX_CTRL_PORT, TX_VIDEO_PORT),
    };
    let ctrl = UdpSocket::bind((ip, cp))?;
    let video = UdpSocket::bind((ip, vp))?;
    let peer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();

    thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = ctrl.recv_from(&mut buf) {
            let req = &buf[..n];
            let reply: Option<Vec<u8>> = match family {
                Family::Text if req.starts_with(b"type=1002") => Some(
                    b"type=2002&w=640&h=480&fps=25&angle=0&ssid=SIM-text&firmware=1&protocol=1\n".to_vec(),
                ),
                Family::Text if req.starts_with(b"type=1001") => Some(b"type=2001&data=77\n".to_vec()),
                Family::Tx if req == [0x10, 0x01] => Some(
                    br#"{"ssid":"SIM-tx","width":640,"height":480,"fps":25,"battery_charging":false,"battery_percentage":64,"firmware_version":2,"device_type":"SIM","company":""}"#.to_vec(),
                ),
                _ => {
                    eprintln!("sim: command {req:02x?}");
                    None
                }
            };
            if let Some(r) = reply {
                let _ = ctrl.send_to(&r, from);
            }
        }
    });

    let rx = video.try_clone()?;
    let p = peer.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = rx.recv_from(&mut buf) {
            let open = matches!(&buf[..n.min(2)], [0x20, 0x36] | [0x20, 0x01]);
            *p.lock().unwrap() = open.then_some(from);
        }
    });

    for frame in 0u32..=u32::MAX {
        thread::sleep(Duration::from_millis(40));
        let Some(to) = *peer.lock().unwrap() else { continue };
        let angle = (30.0 * (frame as f32 * 0.05).sin()).round(); // degrees, wobbling ±30
        for pkt in packets(family, frame as u8, &fake_jpeg(frame), angle) {
            let _ = video.send_to(&pkt, to);
        }
    }
    Ok(())
}

/// 640x480 test pattern: colour gradient, an "UP" arrow and a moving dot.
pub fn fake_jpeg(frame: u32) -> Vec<u8> {
    let (w, h) = (640u32, 480u32);
    let dot = (frame * 8) % w;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let arrow = x.abs_diff(w / 2) < 20 && y < h / 2 || y < 60 && x.abs_diff(w / 2) < 60 - y;
            let is_dot = x.abs_diff(dot) < 15 && y.abs_diff(h - 40) < 15;
            let px = if arrow || is_dot { [255, 255, 255] } else { [(x * 255 / w) as u8, (y * 255 / h) as u8, 120] };
            rgb.extend(px);
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .expect("jpeg encode");
    out
}

pub fn packets(family: Family, id: u8, jpeg: &[u8], angle: f32) -> Vec<Vec<u8>> {
    match family {
        Family::Text => {
            let payload = [&(angle as i16).to_be_bytes()[..], jpeg].concat();
            let chunks: Vec<_> = payload.chunks(CHUNK).collect();
            let last = chunks.len() - 1;
            chunks
                .iter()
                .enumerate()
                .map(|(i, c)| [&[id, (i == last) as u8, 0, i as u8][..], c].concat())
                .collect()
        }
        Family::Tx => {
            let chunks: Vec<_> = jpeg.chunks(CHUNK).collect();
            let n = chunks.len() as u8;
            chunks
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    [&[id, i as u8, n][..], &angle.to_be_bytes(), &[0; 5], c].concat()
                })
                .collect()
        }
    }
}
