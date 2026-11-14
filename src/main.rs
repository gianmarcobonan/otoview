// No console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Usage: otoview [--host 192.168.1.1] [--sim text|tx]

use otoview::app::App;
use otoview::protocol::{Config, Family};
use otoview::sim;
use std::net::{IpAddr, Ipv4Addr};

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    let mut cfg = Config::default();
    if let Some(h) = arg("--host") {
        cfg.host = h.parse().expect("--host: invalid IP");
    }
    if let Some(s) = arg("--sim") {
        let family = match s.as_str() {
            "text" => Family::Text,
            "tx" => Family::Tx,
            _ => panic!("--sim text|tx"),
        };
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        cfg.host = lo;
        cfg.tx_video_local_port = 0;
        std::thread::spawn(move || sim::run(family, lo).expect("simulator"));
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("OtoView")
            .with_inner_size([960.0, 760.0])
            .with_min_inner_size([520.0, 400.0])
            .with_icon(eframe::egui::IconData {
                rgba: include_bytes!("../assets/icon-64.rgba").to_vec(),
                width: 64,
                height: 64,
            }),
        ..Default::default()
    };
    eframe::run_native("OtoView", options, Box::new(move |cc| Ok(Box::new(App::new(cc.egui_ctx.clone(), cfg)))))
}
