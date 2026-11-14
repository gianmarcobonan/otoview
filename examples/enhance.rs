//! Tries the live enhancer on a still image turned into a shaky, noisy, low-quality
//! JPEG burst (like the endoscope stream), and reports the time per frame.
//!   cargo run --release --example enhance -- input.jpg out-prefix
use image::codecs::jpeg::JpegEncoder;
use otoview::enhance::Enhancer;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, out] = &args[..] else {
        eprintln!("usage: enhance <image> <out-prefix>");
        std::process::exit(2);
    };
    let src = image::open(input).expect("read image").to_rgb8();
    let (w, h) = (src.width() - 16, src.height() - 16);
    let mut rng = 0x9E37_79B9u32;
    let mut rand = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };
    let mut e = Enhancer::default();
    let (mut total, mut first, mut last) = (0.0, None, None);
    for i in 0..30 {
        // Hand shake: crop moves by a few pixels; then sensor noise and JPEG at quality 50.
        let (ox, oy) = (8 + (rand() % 7) as i32 - 3, 8 + (rand() % 7) as i32 - 3);
        let mut f = image::imageops::crop_imm(&src, ox as u32, oy as u32, w, h).to_image();
        for p in f.pixels_mut() {
            let n = (rand() % 21) as i32 - 10;
            p.0 = p.0.map(|c| (c as i32 + n).clamp(0, 255) as u8);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 50).encode_image(&f).unwrap();
        let f = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        let t = Instant::now();
        let out = e.process(&f);
        if i >= 5 {
            total += t.elapsed().as_secs_f64();
        }
        first.get_or_insert(f);
        last = Some(out);
    }
    println!("{w}x{h}: {:.1} ms/frame", total / 25.0 * 1000.0);
    first.unwrap().save(format!("{out}-before.png")).unwrap();
    last.unwrap().save(format!("{out}-after.png")).unwrap();
}
