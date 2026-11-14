//! Needs the original APK next to the repo (not redistributed): skipped otherwise.
use otoview::detect::{Detector, MIN_SCORE, extract_model};

#[test]
fn apk_model_runs() {
    let apk = std::path::Path::new("../endscope-tool-2-6-6.apk");
    let Ok(bytes) = std::fs::read(apk) else {
        eprintln!("{} not found: skipping", apk.display());
        return;
    };
    let det = Detector::load(extract_model(bytes).unwrap()).unwrap();
    let grey = image::RgbImage::from_pixel(640, 480, image::Rgb([128, 128, 128]));
    let d = det.detect(&grey).unwrap();
    assert_eq!(d.len(), 10);
    assert!(d.iter().all(|x| x.score < MIN_SCORE), "no eardrum in a grey frame");
}
