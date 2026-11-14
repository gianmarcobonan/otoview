//! cargo run --release --example detect -- <model core.db> <image> [dump-input.f32]
//! Prints the top detections (normalised ymin xmin ymax xmax score).
//! With a 3rd arg, also writes the preprocessed input (raw little-endian f32) for
//! comparison with the reference TFLite interpreter.
use otoview::detect::{Detector, preprocess};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let det = Detector::load(std::fs::read(&a[1]).unwrap()).unwrap();
    let input = preprocess(&image::open(&a[2]).unwrap().to_rgb8());
    if let Some(p) = a.get(3) {
        std::fs::write(p, input.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    }
    let t = std::time::Instant::now();
    let d = det.run(input).unwrap();
    eprintln!("inference {:?}", t.elapsed());
    for x in d {
        println!("{:.4} {:.4} {:.4} {:.4} {:.4}", x.ymin, x.xmin, x.ymax, x.xmax, x.score);
    }
}
