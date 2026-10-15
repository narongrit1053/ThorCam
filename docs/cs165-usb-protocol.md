# Zelux CS165 USB protocol

How the Thorlabs SDK talks to a Zelux CS165MU/CU over USB, recovered from the shipped DLLs, so the camera can be
driven without any Thorlabs DLL (libusb, WinUSB, WebUSB, or the `CYUSB3` driver directly).

**Sources.** *Observed*: `tlcam/examples/usbtrace.rs` runs the real SDK (2.1.0.0, 2022-04-27) and logs every
transfer it hands to the Cypress `CYUSB3` driver (traces of 2026-10-15, CS165CU serial 28764, firmware
`66-ITN004385-IMG v0.9.6`). *Static*: strings and disassembly of `thorlabs_tsi_camera_sdk.dll` and
`thorlabs_tsi_zelux_camera_device.dll`. Everything below is observed unless marked *inferred*.

## Device

| | |
|---|---|
| VID / PID | `0x1313` / `0x4002` (Thorlabs Scientific Imaging, product string "TSI") |
| Chip | Cypress FX3; Windows driver `CYUSB3` (`oem107.inf`, Thorlabs 1.2.3.14), interface GUID `{AE18AA60-7F6A-11D4-97DD-00010229B959}` |
| USB | 3.0 (`bcdUSB 0x0300`), class `0xFF` vendor-specific, 1 configuration |
| Firmware | Already running at enumeration; the host does **not** download firmware. |

| Endpoint | Direction | Use | Transfer size |
|---|---|---|---|
| `0x02` | bulk OUT | command | always 1024 bytes |
| `0x82` | bulk IN | response | always 1024 bytes |
| `0x83` | bulk IN | image data | 16384 bytes per packet |

The SDK calls `0x02`/`0x82` `UART_TX_ENDPOINT`/`UART_RX_ENDPOINT` and `0x83` `DATA_ENDPOINT`; it also names an
`EVENT_ENDPOINT` that it never used in the traces. No control (EP0) transfers are used apart from reading the device
descriptor. All three endpoints are bulk, so any generic USB stack can drive them (no isochronous transfers).

Verified on 2026-10-15: with the camera bound to Microsoft's WinUSB driver instead of `CYUSB3` (via Zadig), a
client written only from this document opened it, read its info and streamed live frames. To give the camera back
to ThorCam, roll back or uninstall that driver in Device Manager and replug.

## Drivers and platforms

| Driver on the camera | Who can open it |
|---|---|
| `CYUSB3` (Thorlabs default, Windows) | Native programs only: the SDK, or any program using the `DeviceIoControl` calls below |
| WinUSB (after a driver swap, Windows) | libusb, WinUSB, and Chromium's WebUSB; no longer ThorCam or the SDK |
| none (Linux, macOS) | libusb / WebUSB directly; Linux needs a udev rule for user access, e.g. `SUBSYSTEM=="usb", ATTR{idVendor}=="1313", ATTR{idProduct}=="4002", MODE="0666"` |

A web page cannot reach a device on `CYUSB3` in any browser. Windows would pick WinUSB by itself only if the camera
firmware reported Microsoft OS descriptors, which this firmware does not. The SDK's own Linux path uses libusb and
mentions `usbfs_memory_mb` in an error message (*inferred*: same protocol over libusb). Not tested on Linux or macOS.

## SDK internals (where the protocol lives in the DLLs)

| DLL | Role |
|---|---|
| `thorlabs_tsi_camera_sdk.dll` | Public `tl_camera_*` API. Turns each call into a text command (`exp 5000`, `gain 240`, …), passes it to the device module as `set_data("command")` / `get_data("response")`, and checks the reply against its error table. Also holds the `cypress_fx3_class` transport used by the CS (Kiralux/Quantalux) path. |
| `thorlabs_tsi_zelux_camera_device.dll` | Zelux device module (internal name `thorlabs_tsi_libusb.dll`). Exports a libusb-compatible API (`libusb_bulk_transfer`, `libusb_submit_transfer`, hotplug, …) implemented on Windows with `SetupDi*` + `CreateFile` + `DeviceIoControl` on the `CYUSB3` device, I/O completion ports for transfers. Names the endpoints `UART_TX_ENDPOINT`, `UART_RX_ENDPOINT`, `DATA_ENDPOINT`, `EVENT_ENDPOINT`. |
| `thorlabs_tsi_usb_hotplug_monitor.dll` | Device arrival/removal events. |
| colour / demosaic / polarization / LUT DLLs | Optional image processing; not involved in the camera protocol. |

`CYUSB3` transfers (CyAPI), as the SDK issues them:

- `IOCTL_ADAPT_SEND_EP0_CONTROL_TRANSFER` = `0x220020` (only for the device descriptor) and
  `IOCTL_ADAPT_SEND_NON_EP0_TRANSFER` = `0x220024` (all bulk traffic), overlapped, with the same buffer as input and
  output.
- The buffer starts with a packed 38-byte `SINGLE_TRANSFER` header: setup packet (`bmRequest u8, bRequest u8,
  wValue u16, wIndex u16, wLength u16, ulTimeOut u32`) at 0, reserved u8 at 12, `ucEndpointAddress` u8 at 13,
  `NtStatus` u32 at 14, `UsbdStatus` u32 at 18, `IsoPacketOffset` u32 at 22, `IsoPacketLength` u32 at 26,
  `BufferOffset` u32 at 30, `BufferLength` u32 at 34; the data follows at `BufferOffset` (38).
- Device path: `\\?\usb#vid_1313&pid_4002#<serial>#{ae18aa60-7f6a-11d4-97dd-00010229b959}`.

So a native program can drive the camera on the default Thorlabs driver with these two IOCTLs and the command
protocol below, without loading any Thorlabs DLL (*inferred*: not yet written and tested).

## Commands

A command is ASCII text, NUL-terminated and zero-padded to **1024 bytes**, written to `0x02`. Every command gets
exactly one response: ASCII text, NUL-terminated, in a 1024-byte read from `0x82`. Arguments are separated by
spaces. A query is the bare name (`exp`); a write adds the value (`exp 5000`) and the response is the value the
camera actually applied.

Errors come back as `error <code>,<text>`, e.g. `error 4,Invalid command`. Codes match the SDK's table: 0 Success,
1 Command not found, 2 Too many arguments, 3 Not enough arguments, 4 Invalid command, 5 Duplicate command,
… Parameter out of range, Command locked, Camera running (parameter cannot be set while in this mode), ROI and bin
combination invalid, Image data sync failure (*inferred* order from the string table in `thorlabs_tsi_camera_sdk.dll`).

The camera describes itself: `discover` lists every command, and `<name>.disc` returns a JSON description, e.g.

```
> gain.disc
< "{"gain": {"desc": "applies gain factor to image","syntax": "gain [dB]","options": [{"factor": "integer","min": 0,"max": 480,"default": 0}],"min": 0,"max": 480,"inc": 1,"default": 0,"flags": ["w"],"response": ["integer"]}}"
```

`<name>.min` / `<name>.max` give ranges.

### Commands the SDK uses (CS165CU responses)

| Command | Response | Meaning |
|---|---|---|
| `id` | `000dc70515000009012b` | unique id (sent during discovery) |
| `sernum` | `28764` | serial number |
| `model` | `CS165CU` | model |
| `ver` | `66-ITN004385-IMG v0.9.6` | firmware version |
| `roi` | `0 0 1439 1079` | ROI: upper-left x, y, lower-right x, y (inclusive) |
| `stop` | `stopped` | stop acquisition (sent on open and on disarm) |
| `color` | `bayer` (`mono` *inferred* for MU) | sensor type |
| `color_filter_phase` | `red` | Bayer phase of pixel (0,0) |
| `color_correction_matrix` | `1.3 -0.3 0.0 -0.45 1.55 -0.1 0.0 -0.2 1.2` | 3×3, row-major |
| `default_white_balance_matrix` | `2.70 0.0 0.0 0.0 1.0 0.0 0.0 0.0 3.15` | 3×3, row-major |
| `pixel_width_nm`, `pixel_height_nm` | `3450` | pixel pitch, nm |
| `pixel_bit_depth` | `10` | |
| `exp`, `exp <µs>` | `5006` | exposure in µs; write returns the applied value (`exp 5000` → `5006`) |
| `exp.min`, `exp.max` | `40`, `26843432` | exposure range, µs |
| `gain`, `gain <n>` | `0` | gain index, 0.1 dB per step (`gain 240` = 24.0 dB per the SDK's `convert_gain_to_decibels`) |
| `gain.min`, `gain.max` | `0`, `480` | |
| `blklvl`, `blklvl <n>` | `1` | black level |
| `blklvl.min`, `blklvl.max` | `0`, `511` | |
| `statusled`, `statusled on\|off` | `on` / `off` | status LED |
| `nframes <n>` | `0` | frames per trigger, 0 = unlimited |
| `data_packets_per_frame` | `121` | image packets per frame (full ROI) |
| `num_calibration_lines` | `6` | lines sent before the image |
| `arm` | `armed` | |
| `start` | `running` | starts streaming (software trigger) |
| `hot_pixel_correction on`, `hwtrig none` | `error 4,Invalid command` | not supported by this firmware; the SDK ignores the error |

### Sequences

- **Discovery:** `id`, `sernum`.
- **Open:** `roi`, `stop`, `discover`, `gain.disc`, `exp.min`, `exp.max`, `color`, `color_filter_phase`,
  `color_correction_matrix`, `default_white_balance_matrix`, `model`, `sernum`, `ver`, `pixel_width_nm`,
  `pixel_height_nm`, `pixel_bit_depth`, `gain.min`, `gain.max`, `blklvl.min`, `blklvl.max`, `exp`, `gain`, `blklvl`,
  `statusled`.
- **Start (free run):** `nframes 0`, `data_packets_per_frame`, `num_calibration_lines`, `roi`, `arm` (≈375 ms),
  `start` → frames stream on `0x83`.
- **Stop:** `stop` (≈100 ms).

## Image data

Each frame arrives as `data_packets_per_frame` packets (121 at full ROI) of 16384 bytes on `0x83`.

Packet header, 16 bytes, little-endian:

| Offset | Type | Value |
|---|---|---|
| 0 | u16 | `0x9090` magic |
| 2 | u16 | packet index 0…120; bit 15 (`0x8000`) set on the last packet of the frame (`0x8078`) |
| 4 | u32 | frame number, from 1 after `start` |
| 8 | u32 | same for every packet of a frame, rises by ≈ the frame period in ms (*inferred*: millisecond timestamp) |
| 12 | u32 | 0 |

Payload: bytes 16…16383 of each packet, concatenated in packet order, form one continuous **MIPI RAW10** stream:
every 5 bytes hold 4 pixels, bytes 0–3 are bits 9…2 of pixels 0–3 and byte 4 holds their 2 low bits (pixel *k* in
bits 2*k*…2*k*+1). The stream is `num_calibration_lines` (6) lines of calibration data followed by the image,
row-major, `width` = 1440 pixels per line; the rest of the last packet is padding.

Verified: for five 200 ms frames this decoding equals the SDK's `tl_camera_get_pending_frame_or_null` output
bit for bit (1440×1080, values 0…1022). The SDK applies no further processing on this camera. A decoder written
from this description also reproduced all five frames exactly when the captured packets were replayed through it.

A client should read with several large transfers in flight (e.g. 8 × 128 KiB): every packet is exactly 16384 bytes,
so packets stay aligned inside larger reads; place each packet by its header (frame number, index) rather than by
arrival order, and drop frames that never complete.

Throughput: 121 × 16 KiB ≈ 1.98 MB per frame, so 35 fps ≈ 69 MB/s.

## Unknowns

- The configuration descriptor was not traced (the SDK does not read it). The WinUSB client found the three
  endpoints in configuration 1; the interface number was not recorded (*inferred*: 0).
- Binning, ROI writes, hardware triggering, `frametime`/`sfr` frame-rate control and `vflip`/`hflip` were not
  exercised; their `<name>.disc` descriptions give the syntax.
- CS165MU responses are inferred from the CU and the SDK strings.
- Other Zelux / CS-series models: the `CS_CAMERA_DEVICE` path in `thorlabs_tsi_camera_sdk.dll` uses a different
  transport (`cypress_fx3_class`) and needs its own trace.
