//! Times the AI photo upscaler on an image and saves the result.
//!   cargo run --release --example upscale -- input.png output.png
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output] = &args[..] else {
        eprintln!("usage: upscale <image> <output>");
        std::process::exit(2);
    };
    let img = image::open(input).expect("read image").to_rgb8();
    let t = std::time::Instant::now();
    let mut up = otoview::upscale::Upscaler::new().expect("load model");
    println!("loaded on {} in {:.1} s", up.device, t.elapsed().as_secs_f32());
    let mut out = None;
    let t = std::time::Instant::now();
    for _ in 0..10 {
        out = Some(up.run(&img).expect("run"));
    }
    let out = out.unwrap();
    println!(
        "{}x{} -> {}x{}: {:.0} ms/photo",
        img.width(),
        img.height(),
        out.width(),
        out.height(),
        t.elapsed().as_secs_f64() * 100.0
    );
    out.save(output).unwrap();
}
