//! Eardrum detector: the original app's SSD-MobileNet TFLite model run with tract (pure Rust).
//!
//! tract cannot run the custom `TFLite_Detection_PostProcess` op, so the model bytes are
//! patched to stop before it and the box decoding + NMS are done here.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tract_tflite::prelude::*;
use tract_tflite::tflite::root_as_model;

const SIZE: u32 = 300;
// From the post-process op's options in the model.
const Y_SCALE: f32 = 10.0;
const X_SCALE: f32 = 10.0;
const H_SCALE: f32 = 5.0;
const W_SCALE: f32 = 5.0;
const NMS_IOU: f32 = 0.6;
const MAX_DETECTIONS: usize = 10;
/// Score the original app requires before drawing a box.
pub const MIN_SCORE: f32 = 0.7;

/// Normalised [0,1] coordinates in the input image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    pub score: f32,
    pub ymin: f32,
    pub xmin: f32,
    pub ymax: f32,
    pub xmax: f32,
}

pub struct Detector {
    plan: Arc<TypedRunnableModel>,
}

/// Drops the trailing post-process op and makes its three inputs (box deltas,
/// class scores, anchors) the subgraph outputs. Done in place: both vectors only shrink.
fn strip_postprocess(mut buf: Vec<u8>) -> Result<Vec<u8>, String> {
    let (ops_len_at, n_ops, outs_at, new_outs) = {
        let m = root_as_model(&buf).map_err(|e| format!("not a TFLite model: {e}"))?;
        let g = m.subgraphs().and_then(|s| s.iter().next()).ok_or("no subgraph")?;
        let ops = g.operators().ok_or("no operators")?;
        let last = ops.get(ops.len() - 1);
        let code = m.operator_codes().ok_or("no opcodes")?.get(last.opcode_index() as usize);
        if code.custom_code() != Some("TFLite_Detection_PostProcess") {
            return Err("unexpected model: last op is not TFLite_Detection_PostProcess".into());
        }
        let ins: Vec<i32> = last.inputs().ok_or("post-process has no inputs")?.iter().collect();
        let outs = g.outputs().ok_or("no outputs")?;
        if ins.len() != 3 || outs.len() < 3 {
            return Err("unexpected post-process signature".into());
        }
        let base = buf.as_ptr() as usize;
        (ops.bytes().as_ptr() as usize - base - 4, ops.len() - 1, outs.bytes().as_ptr() as usize - base - 4, ins)
    };
    buf[ops_len_at..ops_len_at + 4].copy_from_slice(&(n_ops as u32).to_le_bytes());
    buf[outs_at..outs_at + 4].copy_from_slice(&3u32.to_le_bytes());
    for (i, t) in new_outs.iter().enumerate() {
        let at = outs_at + 4 + 4 * i;
        buf[at..at + 4].copy_from_slice(&t.to_le_bytes());
    }
    Ok(buf)
}

impl Detector {
    /// `bytes`: the model file (`assets/flutter_assets/assets/data/core.db` in the APK).
    pub fn load(bytes: Vec<u8>) -> Result<Detector, String> {
        let buf = strip_postprocess(bytes)?;
        let plan = (|| -> TractResult<_> {
            tract_tflite::tflite()
                .model_for_read(&mut &buf[..])?
                .into_optimized()?
                .into_runnable()
        })()
        .map_err(|e| format!("{e:#}"))?;
        Ok(Detector { plan })
    }

    /// Top detections after NMS (unfiltered; compare `score` with `MIN_SCORE`).
    pub fn detect(&self, img: &image::RgbImage) -> Result<Vec<Detection>, String> {
        self.run(preprocess(img))
    }

    /// `input`: 300*300*3 floats, HWC RGB, from `preprocess`.
    pub fn run(&self, input: Vec<f32>) -> Result<Vec<Detection>, String> {
        let input = tract_ndarray::Array4::from_shape_vec((1, 300, 300, 3), input).map_err(|e| e.to_string())?;
        let out = self.plan.run(tvec!(Tensor::from(input).into())).map_err(|e| format!("{e:#}"))?;
        let flat = |i: usize| -> Result<Vec<f32>, String> {
            Ok(out[i].to_plain_array_view::<f32>().map_err(|e| e.to_string())?.iter().copied().collect())
        };
        Ok(decode(&flat(0)?, &flat(1)?, &flat(2)?))
    }
}

const APK_MODEL: &str = "assets/flutter_assets/assets/data/core.db";

/// Per-user copy of the imported model (the portable bundle may be read-only).
pub fn model_path() -> PathBuf {
    let env = |k| std::env::var_os(k).map(PathBuf::from);
    let home = env("HOME").or_else(|| env("USERPROFILE")).unwrap_or_default();
    let base = if cfg!(windows) {
        env("APPDATA").unwrap_or(home)
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        env("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"))
    };
    base.join("otoview").join("model.tflite")
}

/// The model is not redistributed: the user supplies the original APK (or its `core.db`).
/// Validates it, stores a copy at `model_path()` and returns the ready detector.
pub fn import(path: &Path) -> Result<Detector, String> {
    let model = extract_model(std::fs::read(path).map_err(|e| e.to_string())?)?;
    let det = Detector::load(model.clone())?;
    let dest = model_path();
    std::fs::create_dir_all(dest.parent().unwrap()).and_then(|_| std::fs::write(&dest, model)).map_err(|e| e.to_string())?;
    Ok(det)
}

/// APK (zip) -> its `core.db`; anything else is returned as-is (a bare model file).
pub fn extract_model(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    Ok(if bytes.starts_with(b"PK") {
        let mut apk = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
        let mut entry = apk.by_name(APK_MODEL).map_err(|_| format!("l'APK non contiene {APK_MODEL}"))?;
        let mut out = Vec::new();
        entry.read_to_end(&mut out).map_err(|e| e.to_string())?;
        out
    } else {
        bytes
    })
}

/// Same preprocessing as the app: resize to 300x300, (v - 127.5) / 127.5, HWC RGB.
pub fn preprocess(img: &image::RgbImage) -> Vec<f32> {
    let small = image::imageops::resize(img, SIZE, SIZE, image::imageops::FilterType::Triangle);
    small.as_raw().iter().map(|&v| (v as f32 - 127.5) / 127.5).collect()
}

/// TFLite_Detection_PostProcess, fast-NMS path, single class.
/// `deltas`/`anchors`: N×[y, x, h, w]; `scores`: N×[background, eardrum] (sigmoid).
pub fn decode(deltas: &[f32], scores: &[f32], anchors: &[f32]) -> Vec<Detection> {
    let n = anchors.len() / 4;
    let mut cands: Vec<Detection> = (0..n)
        .map(|i| {
            let (d, a) = (&deltas[4 * i..4 * i + 4], &anchors[4 * i..4 * i + 4]);
            let yc = d[0] / Y_SCALE * a[2] + a[0];
            let xc = d[1] / X_SCALE * a[3] + a[1];
            let h = (d[2] / H_SCALE).exp() * a[2];
            let w = (d[3] / W_SCALE).exp() * a[3];
            Detection { score: scores[2 * i + 1], ymin: yc - h / 2.0, xmin: xc - w / 2.0, ymax: yc + h / 2.0, xmax: xc + w / 2.0 }
        })
        .collect();
    cands.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    for c in cands {
        if kept.len() == MAX_DETECTIONS {
            break;
        }
        if kept.iter().all(|k| iou(k, &c) <= NMS_IOU) {
            kept.push(c);
        }
    }
    kept
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let area = |d: &Detection| (d.ymax - d.ymin).max(0.0) * (d.xmax - d.xmin).max(0.0);
    let ih = (a.ymax.min(b.ymax) - a.ymin.max(b.ymin)).max(0.0);
    let iw = (a.xmax.min(b.xmax) - a.xmin.max(b.xmin)).max(0.0);
    let inter = ih * iw;
    let union = area(a) + area(b) - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_identity_and_nms() {
        // Two anchors at the same place, zero deltas -> boxes equal to anchors; NMS keeps one.
        let anchors = [0.5, 0.5, 0.2, 0.4, 0.5, 0.5, 0.2, 0.4, 0.1, 0.1, 0.1, 0.1];
        let deltas = [0.0; 12];
        let scores = [0.0, 0.9, 0.0, 0.8, 0.0, 0.3];
        let d = decode(&deltas, &scores, &anchors);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], Detection { score: 0.9, ymin: 0.4, xmin: 0.3, ymax: 0.6, xmax: 0.7 });
        assert_eq!(d[1].score, 0.3);
    }
}
