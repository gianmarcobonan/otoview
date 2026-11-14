//! cargo run --example wifi : current SSID + endoscopes in range (read-only).
fn main() {
    println!("current: {:?}", otoview::wifi::current_ssid());
    let t = std::time::Instant::now();
    println!("endoscopes: {:?} ({:?})", otoview::wifi::scan(), t.elapsed());
}
