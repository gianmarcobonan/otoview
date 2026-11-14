//! AI super-resolution of photos: Real-ESRGAN "general v3" (BSD-3-Clause, see
//! `tools/convert_esrgan.py`) through ONNX Runtime, on the GPU when the system offers one
//! (DirectML on Windows, CoreML on Apple Silicon, WebGPU/Vulkan on Linux), else on the CPU.
//!
//! Photos only: about 1 s per 640×480 frame on integrated graphics, far from live. The
//! network invents plausible texture (it was seen rewriting letters), so its output must
//! always be labelled as AI and kept next to the original.

use image::{RgbImage, imageops};

/// Largest network input in pixels (the network costs ~2.4 MFLOP per input pixel).
const MAX_INPUT_PIXELS: f32 = 1280.0 * 720.0;

/// Network input size: the full frame, scaled down only above `MAX_INPUT_PIXELS`.
/// Never halved first: detail lost before the network gets invented back.
fn input_size(w: u32, h: u32) -> (u32, u32) {
    let scale = (MAX_INPUT_PIXELS / (w * h) as f32).sqrt().min(1.0);
    (((w as f32 * scale) as u32).max(8), ((h as f32 * scale) as u32).max(8))
}

/// The network gives 4×; photos keep 2× of the original, downsampled from it.
fn finish(img: &RgbImage, out4: RgbImage) -> RgbImage {
    imageops::resize(&out4, img.width() * 2, img.height() * 2, imageops::FilterType::Lanczos3)
}

#[cfg(any(windows, all(target_os = "linux", target_arch = "x86_64"), all(target_os = "macos", target_arch = "aarch64")))]
mod imp {
    use super::*;
    use ort::ep;
    use ort::session::Session;
    use ort::value::Tensor;

    static MODEL: &[u8] = include_bytes!("../assets/esrgan-x4.onnx");

    pub struct Upscaler {
        session: Session,
        /// Where the network runs, for the UI ("GPU (DirectML)", "CPU").
        pub device: &'static str,
    }

    fn gpu() -> (ep::ExecutionProviderDispatch, &'static str) {
        #[cfg(windows)]
        return (ep::DirectML::default().build(), "GPU (DirectML)");
        #[cfg(target_os = "macos")]
        return (ep::CoreML::default().build(), "GPU (CoreML)");
        #[cfg(target_os = "linux")]
        return (ep::WebGPU::default().build(), "GPU (Vulkan)");
    }

    impl Upscaler {
        /// Loads the network, on the GPU if possible. Takes seconds: call off the UI thread.
        pub fn new() -> Result<Upscaler, String> {
            let env = ort::init().with_name("otoview").build().map_err(|e| e.to_string())?;
            let (ep, name) = gpu();
            // OTOVIEW_AI_CPU=1 forces the CPU (troubleshooting, benchmarks).
            let on_gpu = if std::env::var_os("OTOVIEW_AI_CPU").is_some() {
                Err("CPU forced".to_string())
            } else {
                Session::builder(&env)
                    .map_err(|e| e.to_string())
                    .and_then(|b| b.with_execution_providers([ep.error_on_failure()]).map_err(|e| e.to_string()))
                    .and_then(|mut b| b.commit_from_memory(MODEL).map_err(|e| e.to_string()))
            };
            let (session, device) = match on_gpu {
                Ok(s) => (s, name),
                Err(_) => {
                    let s = Session::builder(&env).and_then(|mut b| b.commit_from_memory(MODEL)).map_err(|e| e.to_string())?;
                    (s, "CPU")
                }
            };
            Ok(Upscaler { session, device })
        }

        /// The photo redrawn by the network at twice its size.
        pub fn run(&mut self, img: &RgbImage) -> Result<RgbImage, String> {
            let (w, h) = input_size(img.width(), img.height());
            let small;
            let src = if (w, h) == img.dimensions() {
                img
            } else {
                small = imageops::resize(img, w, h, imageops::FilterType::Triangle);
                &small
            };
            let (wu, hu) = (w as usize, h as usize);
            let mut planar = vec![0f32; 3 * wu * hu];
            for (i, p) in src.pixels().enumerate() {
                for c in 0..3 {
                    planar[c * wu * hu + i] = p.0[c] as f32 / 255.0;
                }
            }
            let input = Tensor::from_array(([1usize, 3, hu, wu], planar)).map_err(|e| e.to_string())?;
            let outputs = self.session.run(ort::inputs!["input" => input]).map_err(|e| e.to_string())?;
            let (shape, data) = outputs["output"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            let (oh, ow) = (shape[2] as usize, shape[3] as usize);
            let plane = ow * oh;
            let out4 = RgbImage::from_fn(ow as u32, oh as u32, |x, y| {
                let i = y as usize * ow + x as usize;
                image::Rgb([0, 1, 2].map(|c| (data[c * plane + i] * 255.0).round().clamp(0.0, 255.0) as u8))
            });
            Ok(finish(img, out4))
        }
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"), all(target_os = "macos", target_arch = "aarch64"))))]
mod imp {
    use super::*;

    /// No prebuilt ONNX Runtime for this system (e.g. Intel Macs).
    pub struct Upscaler {
        pub device: &'static str,
    }

    impl Upscaler {
        pub fn new() -> Result<Upscaler, String> {
            let _ = (input_size, finish);
            Err(crate::tr("AI non disponibile su questo sistema", "AI is not available on this system").into())
        }

        pub fn run(&mut self, _img: &RgbImage) -> Result<RgbImage, String> {
            unreachable!("new() always fails")
        }
    }
}

pub use imp::Upscaler;

#[cfg(test)]
mod tests {
    #[test]
    fn input_is_full_frame_up_to_720p() {
        assert_eq!(super::input_size(640, 480), (640, 480));
        let (w, h) = super::input_size(1920, 1080);
        assert!(w * h <= 1280 * 720 && (w as f32 / h as f32 - 16.0 / 9.0).abs() < 0.02, "{w}x{h}");
    }

    #[cfg(windows)]
    #[test]
    fn doubles_the_photo() {
        let mut up = super::Upscaler::new().unwrap();
        let out = up.run(&image::RgbImage::from_pixel(120, 80, image::Rgb([90, 120, 150]))).unwrap();
        assert_eq!(out.dimensions(), (240, 160));
        let p = out.get_pixel(120, 80).0;
        assert!(p.iter().zip([90, 120, 150]).all(|(a, b)| (*a as i32 - b).abs() < 12), "flat colour changed: {p:?}");
    }
}
