// WebUSB driver for the Thorlabs Zelux CS165MU/CU, written from the traced protocol (docs/cs165-usb-protocol.md).
// No Thorlabs code or DLL is involved: the camera speaks plain ASCII commands over bulk endpoints.
//
//   commands:  bulk OUT 0x02, ASCII + NUL, zero-padded to 1024 bytes
//   responses: bulk IN  0x82, 1024 bytes, ASCII + NUL ("error <code>,<text>" on failure)
//   images:    bulk IN  0x83, 16384-byte packets: 16-byte header (0x9090, index | 0x8000 on the last packet,
//              frame number, stamp) + MIPI RAW10 payload; num_calibration_lines lines precede the image.

export const FILTERS = [{ vendorId: 0x1313, productId: 0x4002 }];

const CMD_SIZE = 1024;
const PACKET = 16384;
const HEADER = 16;
const MAGIC = 0x9090;
// Image reads: several packets per transfer and several transfers in flight keep up with ~70 MB/s.
const PACKETS_PER_READ = 8;
const READS_IN_FLIGHT = 8;
const CFA = { red: 0, blue: 1, green_left_of_red: 2, green_left_of_blue: 3 };

export class CameraError extends Error {}

/** Is WebUSB available in this browser? */
export function supported() {
  return typeof navigator !== 'undefined' && 'usb' in navigator;
}

/** Cameras this site was already allowed to use (no prompt). */
export async function pairedCameras() {
  if (!supported()) return [];
  const devs = await navigator.usb.getDevices();
  return devs.filter((d) => FILTERS.some((f) => f.vendorId === d.vendorId && f.productId === d.productId));
}

/** Asks the user to pick a camera (needs a click). */
export async function requestCamera() {
  return navigator.usb.requestDevice({ filters: FILTERS });
}

const enc = new TextEncoder();
const dec = new TextDecoder('ascii');

function parseMatrix(s) {
  const v = s.trim().split(/\s+/).map(Number);
  return v.length === 9 && v.every(Number.isFinite) ? v : null;
}

/** 3 x 3 row-major product a × b. */
function matmul(a, b) {
  return Array.from({ length: 9 }, (_, i) => {
    const r = Math.floor(i / 3), c = i % 3;
    return a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
  });
}

export class ZeluxCamera {
  /** @param {USBDevice} device */
  constructor(device) {
    this.device = device;
    this.info = null;
    this.running = false;
    this.queue = Promise.resolve();
    this.stats = { bytes: 0, packets: 0, badPackets: 0, framesIncomplete: 0 };
  }

  async open() {
    const d = this.device;
    if (!d.opened) await d.open();
    if (d.configuration === null) await d.selectConfiguration(1);
    // The interface that has the command endpoint.
    const iface = d.configuration.interfaces.find((i) =>
      i.alternates.some((a) => a.endpoints.some((e) => e.endpointNumber === 2 && e.direction === 'out')));
    if (!iface) throw new CameraError('This device has no command endpoint; is it a Zelux camera?');
    this.ifaceNumber = iface.interfaceNumber;
    await d.claimInterface(this.ifaceNumber);
    await this.command('stop');
    this.info = await this.readInfo();
    return this.info;
  }

  /** Sends one command and returns the camera's response text. Commands are serialised. */
  command(text) {
    const run = async () => {
      const buf = new Uint8Array(CMD_SIZE);
      buf.set(enc.encode(text).subarray(0, CMD_SIZE - 1));
      const out = await this.device.transferOut(2, buf);
      if (out.status !== 'ok') throw new CameraError(`USB write failed (${out.status}) for "${text}"`);
      const res = await this.device.transferIn(2, CMD_SIZE);
      if (res.status !== 'ok') throw new CameraError(`USB read failed (${res.status}) for "${text}"`);
      const bytes = new Uint8Array(res.data.buffer, res.data.byteOffset, res.data.byteLength);
      const end = bytes.indexOf(0);
      const reply = dec.decode(bytes.subarray(0, end < 0 ? bytes.length : end)).trim();
      if (reply.startsWith('error')) throw new CameraError(`"${text}": ${reply}`);
      return reply;
    };
    const p = this.queue.then(run, run);
    this.queue = p.catch(() => {});
    return p;
  }

  async tryCommand(text) {
    try {
      return await this.command(text);
    } catch {
      return null;
    }
  }

  async readInfo() {
    const q = (c) => this.command(c);
    const num = async (c, fallback) => {
      const v = Number(await this.tryCommand(c));
      return Number.isFinite(v) ? v : fallback;
    };
    const roi = (await q('roi')).split(/\s+/).map(Number);
    const color = (await this.tryCommand('color')) ?? 'mono';
    const info = {
      model: await q('model'),
      serial: await q('sernum'),
      firmware: (await this.tryCommand('ver')) ?? '',
      width: roi[2] - roi[0] + 1,
      height: roi[3] - roi[1] + 1,
      bitDepth: await num('pixel_bit_depth', 10),
      pixelWUm: (await num('pixel_width_nm', 3450)) / 1000,
      pixelHUm: (await num('pixel_height_nm', 3450)) / 1000,
      color: color === 'bayer',
      cfa: -1,
      matrix: [1, 0, 0, 0, 1, 0, 0, 0, 1],
      exposureRangeUs: [await num('exp.min', 40), await num('exp.max', 26843432)],
      gainRange: [await num('gain.min', 0), await num('gain.max', 0)],
      blackLevelRange: [await num('blklvl.min', 0), await num('blklvl.max', 0)],
    };
    if (info.color) {
      info.cfa = CFA[(await this.tryCommand('color_filter_phase')) ?? 'red'] ?? 0;
      const ccm = parseMatrix((await this.tryCommand('color_correction_matrix')) ?? '');
      const wb = parseMatrix((await this.tryCommand('default_white_balance_matrix')) ?? '');
      // Display matrix = colour correction × white balance, as tlcam::ColorInfo::display_matrix.
      info.matrix = matmul(ccm ?? info.matrix, wb ?? info.matrix);
    }
    info.ledSupported = (await this.tryCommand('statusled')) !== null;
    info.applied = await this.readBack(info);
    return info;
  }

  async readBack(info = this.info) {
    const gain = Number(await this.tryCommand('gain')) || 0;
    return {
      exposureMs: Number(await this.command('exp')) / 1000,
      gain,
      gainDb: gain / 10,
      blackLevel: Number(await this.tryCommand('blklvl')) || 0,
      ledOn: info.ledSupported ? (await this.tryCommand('statusled')) === 'on' : false,
    };
  }

  /** Writes the settings (clamped to the camera's ranges) and returns what the camera applied. */
  async apply(s) {
    const i = this.info;
    const clamp = (v, [lo, hi]) => Math.min(Math.max(v, lo), Math.max(hi, lo));
    const write = async () => {
      await this.command(`exp ${Math.round(clamp(s.exposureMs * 1000, i.exposureRangeUs))}`);
      if (i.gainRange[1] > i.gainRange[0]) await this.command(`gain ${Math.round(clamp(s.gain, i.gainRange))}`);
      if (s.blackLevel != null && i.blackLevelRange[1] > i.blackLevelRange[0]) {
        await this.command(`blklvl ${Math.round(clamp(s.blackLevel, i.blackLevelRange))}`);
      }
      if (i.ledSupported) await this.command(`statusled ${s.ledOn ? 'on' : 'off'}`);
    };
    try {
      await write();
    } catch (e) {
      // Some settings cannot change while running: stop, write, restart (as tlcam::Camera::apply).
      if (!this.running) throw e;
      const onFrame = this.onFrame;
      await this.stop();
      try {
        await write();
      } finally {
        await this.start(onFrame);
      }
    }
    i.applied = await this.readBack();
    return i.applied;
  }

  /**
   * Free-running acquisition. `onFrame({frame, stamp, stream, calibrationLines})` gets each complete frame:
   * `stream` is an ArrayBuffer of the joined packet payloads (RAW10).
   */
  async start(onFrame) {
    this.onFrame = onFrame;
    await this.command('nframes 0');
    this.packetsPerFrame = Number(await this.command('data_packets_per_frame'));
    this.calibrationLines = Number((await this.tryCommand('num_calibration_lines')) ?? 0);
    await this.command('arm');
    this.running = true;
    this.frames = new Map();
    this.lastEmitted = 0;
    // Reads still pending from an earlier run carry on; only top up to READS_IN_FLIGHT.
    this.activeReaders ??= 0;
    this.readers = Array.from({ length: Math.max(0, READS_IN_FLIGHT - this.activeReaders) }, () => this.readLoop());
    await this.command('start');
  }

  async readLoop() {
    this.activeReaders++;
    try {
      await this.readPackets();
    } finally {
      this.activeReaders--;
    }
  }

  async readPackets() {
    while (this.running) {
      let r;
      try {
        r = await this.device.transferIn(3, PACKET * PACKETS_PER_READ);
      } catch (e) {
        if (!this.running) return;
        this.running = false;
        this.onError?.(e);
        return;
      }
      if (r.status === 'stall') {
        await this.device.clearHalt('in', 3).catch(() => {});
        continue;
      }
      if (!r.data || r.data.byteLength === 0) continue;
      this.stats.bytes += r.data.byteLength;
      const buf = new Uint8Array(r.data.buffer, r.data.byteOffset, r.data.byteLength);
      for (let off = 0; off + HEADER <= buf.length; off += PACKET) this.onPacket(buf.subarray(off, Math.min(off + PACKET, buf.length)));
    }
  }

  onPacket(p) {
    const dv = new DataView(p.buffer, p.byteOffset, p.byteLength);
    if (p.length !== PACKET || dv.getUint16(0, true) !== MAGIC) {
      this.stats.badPackets++;
      return;
    }
    this.stats.packets++;
    const idx = dv.getUint16(2, true) & 0x7fff;
    const frameNo = dv.getUint32(4, true);
    const stamp = dv.getUint32(8, true);
    if (frameNo <= this.lastEmitted) return;
    let f = this.frames.get(frameNo);
    if (!f) {
      f = { parts: new Array(this.packetsPerFrame), have: 0, stamp };
      this.frames.set(frameNo, f);
    }
    if (idx >= this.packetsPerFrame || f.parts[idx]) return;
    // Copy: the transfer's buffer is reused for the next packets of the read.
    f.parts[idx] = p.slice(HEADER);
    f.have++;
    if (f.have === this.packetsPerFrame) {
      this.frames.delete(frameNo);
      // Older unfinished frames will never complete.
      for (const k of this.frames.keys()) {
        if (k < frameNo) {
          this.frames.delete(k);
          this.stats.framesIncomplete++;
        }
      }
      this.lastEmitted = frameNo;
      const payload = PACKET - HEADER;
      const stream = new Uint8Array(payload * this.packetsPerFrame);
      f.parts.forEach((part, i) => stream.set(part, i * payload));
      this.onFrame?.({ frame: frameNo, stamp: f.stamp, stream: stream.buffer, calibrationLines: this.calibrationLines });
    }
  }

  async stop() {
    if (!this.running) return;
    this.running = false;
    await this.tryCommand('stop');
    // Pending image reads end once the camera stops sending; do not wait on them forever.
    await Promise.race([Promise.allSettled(this.readers ?? []), new Promise((r) => setTimeout(r, 500))]);
    this.frames?.clear();
  }

  async close() {
    try {
      await this.stop();
      await this.device.releaseInterface(this.ifaceNumber).catch(() => {});
    } finally {
      await this.device.close().catch(() => {});
    }
  }
}
