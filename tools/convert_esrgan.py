# /// script
# requires-python = ">=3.10"
# dependencies = ["torch>=2.4", "onnx>=1.16"]
# ///
"""Builds assets/esrgan-x4.onnx from the official Real-ESRGAN "general v3" weights.

    uv run tools/convert_esrgan.py [denoise=0.5]

Downloads realesr-general-x4v3 and its "without denoise" twin from the Real-ESRGAN
release (BSD-3-Clause, https://github.com/xinntao/Real-ESRGAN), checks their SHA-256,
blends them like Real-ESRGAN's --denoise_strength, and exports an ONNX model:
input/output float32 RGB in 0..1, NCHW, any size, output 4x larger.
"""

import hashlib
import sys
import urllib.request
from pathlib import Path

import torch
from torch import nn
from torch.nn import functional as F

RELEASE = "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/"
WEIGHTS = {
    "realesr-general-x4v3.pth": "8dc7edb9ac80ccdc30c3a5dca6616509367f05fbc184ad95b731f05bece96292",
    "realesr-general-wdn-x4v3.pth": "1641f8c4464b9f097c9fdda5589273713f67cf59f3d909e0bd688f0cee269dca",
}
OUT = Path(__file__).resolve().parent.parent / "assets" / "esrgan-x4.onnx"


class SRVGGNetCompact(nn.Module):
    """Real-ESRGAN's compact network (same layer names as the checkpoints)."""

    def __init__(self, feat=64, convs=32, scale=4):
        super().__init__()
        self.scale = scale
        layers = [nn.Conv2d(3, feat, 3, 1, 1), nn.PReLU(feat)]
        for _ in range(convs):
            layers += [nn.Conv2d(feat, feat, 3, 1, 1), nn.PReLU(feat)]
        layers.append(nn.Conv2d(feat, 3 * scale * scale, 3, 1, 1))
        self.body = nn.ModuleList(layers)
        self.upsampler = nn.PixelShuffle(scale)

    def forward(self, x):
        out = x
        for layer in self.body:
            out = layer(out)
        return self.upsampler(out) + F.interpolate(x, scale_factor=self.scale, mode="nearest")


def weights(name: str) -> dict:
    path = Path(__file__).resolve().parent.parent / "target" / name
    if not path.exists():
        path.parent.mkdir(exist_ok=True)
        urllib.request.urlretrieve(RELEASE + name, path)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != WEIGHTS[name]:
        sys.exit(f"{name}: unexpected SHA-256 {digest}")
    state = torch.load(path, map_location="cpu", weights_only=True)
    return state.get("params_ema", state.get("params", state))


def main():
    denoise = float(sys.argv[1]) if len(sys.argv) > 1 else 0.5
    a, b = weights("realesr-general-x4v3.pth"), weights("realesr-general-wdn-x4v3.pth")
    net = SRVGGNetCompact().eval()
    net.load_state_dict({k: denoise * a[k] + (1 - denoise) * b[k] for k in a})
    x = torch.rand(1, 3, 64, 64)
    torch.onnx.export(
        net, x, OUT, input_names=["input"], output_names=["output"], opset_version=17, dynamo=False,
        dynamic_axes={"input": {2: "h", 3: "w"}, "output": {2: "h4", 3: "w4"}},
    )
    print(f"wrote {OUT} ({OUT.stat().st_size // 1024} KiB, denoise {denoise})")


if __name__ == "__main__":
    main()
