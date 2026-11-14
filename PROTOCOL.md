# Wi-Fi endoscope protocol

How the Wi-Fi ear endoscopes supported by the *Endscope Tool* Android app (v2.6.6) talk to the phone.
Reconstructed from a static analysis of that app, for interoperability; implemented in [`src/protocol.rs`](src/protocol.rs).

**Observed** means read in the app's code. **Inferred** means interpretation still to be confirmed on real hardware.

## Network

- The device is a Wi-Fi access point: open network named `softish-xxxx`, fixed IP **192.168.1.1**.
- Everything is **UDP**. There is no TCP or HTTP towards the device.
- There are **two firmware families**. The app probes both and uses whichever answers.

| | **Text** family | **TX** family (binary + JSON) |
|---|---|---|
| Local control socket | `0.0.0.0:50000+rand(0..499)` | `0.0.0.0:40000+rand(0..499)` |
| Device control port | **61502** | **61500** |
| Local video socket | `0.0.0.0:51000+rand(0..499)` | `0.0.0.0:`**61501** (fixed) |
| Device video port | **61503** | **61501** |

The device replies to the source address and port of each request,
so video commands must be sent **from the video socket** and control commands **from the control socket**.

## Lifecycle

1. Open the four sockets (two per family).
2. Every **100 ms**, until an info reply arrives: send `getInfo` to **both** families.
3. Every **1 s** after that: `getBattery`.
4. Every **500 ms**, watchdog: no video for 2 s means disconnected; no control reply for 2 s means back to step 2.
5. `openCamera` when the live view is shown, `closeCamera` when it is left.

## Commands

Text commands are ASCII and end with `\n`.

| Function | Text (to :61502 from the control socket) | TX (to :61500 from the control socket) |
|---|---|---|
| getInfo | `type=1002\n` | `10 01` |
| getBattery | `type=1001\n` | `10 01` (same request; the JSON reply carries the battery) |
| getLogo | `type=1004\n` | n/a |
| setLed(v) | `type=1003&value=<v>\n`, v clamped to 0..100 | `10 02 <v_hi> <v_lo>` (uint16, big-endian) |
| openCamera | `20 36`, plus `<p_hi> <p_lo>` if protocol ≥ 2, **to :61503 from the video socket** | `20 01` **to :61501 from the video socket** |
| closeCamera | `20 37` **to :61503 from the video socket** | `20 02` **to :61501 from the video socket** |

`protocol` (`p`) comes from the Text family's info reply (default 1).

## Control replies

**Text** family: a query string (newlines removed):

- `type=2001&data=<battery %>`
- `type=2002&w=<int>&h=<int>&fps=<int>&angle=<int>&ssid=<str>&firmware=<int>&protocol=<int>&logo=<int>&company=<str>&dn=<str>`
- Binary packets starting with `"LOGO"` (`4C 4F 47 4F`): chunks of the maker's logo image (OtoView ignores them).

**TX** family: UTF-8 JSON, for example

```json
{"ssid": "...", "width": 640, "height": 480, "fps": 25, "battery_percentage": 80,
 "battery_charging": false, "firmware_version": 2, "device_type": "...", "company": "..."}
```

When `battery_charging` is true the app shows the battery as charging.

## Video: JPEG frames split over UDP

Each frame is one JPEG split into several datagrams. Reassemble the payloads in order,
then take the bytes from the first `FF D8` to the last `FF D9`.

**Text** family (datagrams longer than 8 bytes):

| Offset | Field |
|---|---|
| 0 | frame id |
| 1 | `1` = last fragment (Inferred: triggers reassembly) |
| 2 | unknown |
| 3 | fragment index (sort key) |
| 4–5 | int16 big-endian: G-sensor angle of the frame (read from the first fragment) |
| 4… | payload = `data[4..]`, so the two angle bytes precede `FF D8` in the first fragment |

Fragments older than 200 ms are dropped.

**TX** family:

| Offset | Field |
|---|---|
| 0 | frame id |
| 1 | fragment index (0 = new frame, clears the buffer) |
| 2 | fragment count (reassemble once that many fragments have arrived) |
| 3–6 | float32 big-endian: rotation angle |
| 7–11 | unknown |
| 12… | JPEG payload |

## Eardrum detector (original app)

- Model: SSD-MobileNet, input 300×300 RGB float, normalised as `(v - 127.5) / 127.5`, one class.
- Final op: `TFLite_Detection_PostProcess` (fast NMS, IoU 0.6, max 10 detections, scales y/x 10, h/w 5).
- The app draws a box when the score is ≥ 0.7.

## Still to confirm on hardware

- Meaning of byte 2 in the Text family and bytes 7–11 in the TX family.
- Unit (degrees?) and direction of the angle.
- Light level range on TX firmware.
