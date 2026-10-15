// Replays a usbtrace capture (tlcam/examples/usbtrace.rs) through the WebUSB driver and the WebAssembly decoder,
// and checks the decoded frames against the frames the Thorlabs SDK returned in the same capture.
//
//   node web/test/replay.mjs <trace base path>     e.g. .../trace2  (needs trace2.log, .raw, .frame1.u16 ...)

import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ZeluxCamera } from '../js/cs165.js';

const base = process.argv[2];
if (!base) {
  console.error('usage: node replay.mjs <trace base path>');
  process.exit(2);
}

// Commands and responses from the log: "-> BULK OUT ep=0x02 ..." followed by "<- BULK IN  ep=0x82 ...".
const text = (hex) => {
  const b = Buffer.from(hex.trim().split(/\s+/).map((h) => parseInt(h, 16)));
  const z = b.indexOf(0);
  return b.subarray(0, z < 0 ? b.length : z).toString('ascii');
};
const responses = new Map();
let pendingCmd = null;
for (const line of readFileSync(`${base}.log`, 'utf8').split('\n')) {
  const out = line.match(/-> BULK OUT ep=0x02 len=\d+: ((?:[0-9a-f]{2} )+)/);
  if (out) pendingCmd = text(out[1]);
  const inn = line.match(/<- BULK IN  ep=0x82 .*got=\d+: ((?:[0-9a-f]{2} )+)/);
  if (inn && pendingCmd != null) {
    responses.set(pendingCmd, text(inn[1]));
    pendingCmd = null;
  }
}

// Image packets, in capture order.
const raw = readFileSync(`${base}.raw`);
const packets = [];
for (let i = 0; i < raw.length; ) {
  const n = raw.readUInt32LE(i);
  packets.push(raw.subarray(i + 4, i + 4 + n));
  i += 4 + n;
}

class MockDevice {
  constructor() {
    this.opened = false;
    this.configuration = {
      interfaces: [{
        interfaceNumber: 0,
        alternates: [{ endpoints: [
          { endpointNumber: 2, direction: 'out' }, { endpointNumber: 2, direction: 'in' }, { endpointNumber: 3, direction: 'in' },
        ] }],
      }],
    };
    this.lastCmd = null;
    this.streaming = false;
    this.next = 0;
    this.log = [];
  }
  async open() { this.opened = true; }
  async close() { this.opened = false; }
  async selectConfiguration() {}
  async claimInterface() {}
  async releaseInterface() {}
  async clearHalt() {}
  async transferOut(ep, data) {
    if (ep !== 2 || data.byteLength !== 1024) throw new Error(`unexpected OUT ep ${ep} len ${data.byteLength}`);
    this.lastCmd = text(Buffer.from(data).toString('hex').match(/../g).join(' '));
    this.log.push(this.lastCmd);
    if (this.lastCmd === 'start') this.streaming = true;
    if (this.lastCmd === 'stop') this.streaming = false;
    return { status: 'ok', bytesWritten: data.byteLength };
  }
  async transferIn(ep, len) {
    if (ep === 2) {
      const c = this.lastCmd;
      let r = responses.get(c);
      if (r === undefined) {
        const [name, arg] = c.split(' ');
        r = arg !== undefined && responses.has(name) ? arg : 'error 1,Command not found';
      }
      const b = new Uint8Array(1024);
      b.set(Buffer.from(r, 'ascii'));
      return { status: 'ok', data: new DataView(b.buffer) };
    }
    if (ep === 3) {
      if (!this.streaming || this.next >= packets.length) {
        await new Promise((r) => setTimeout(r, 5));
        return { status: 'ok', data: new DataView(new ArrayBuffer(0)) };
      }
      const k = Math.min(len / 16384, packets.length - this.next);
      const b = Buffer.concat(packets.slice(this.next, this.next + k));
      this.next += k;
      return { status: 'ok', data: new DataView(b.buffer, b.byteOffset, b.byteLength) };
    }
    throw new Error(`unexpected IN ep ${ep}`);
  }
}

// WebAssembly decoder, as the worker uses it.
const wasmPath = fileURLToPath(new URL('../wasm/beam_wasm.wasm', import.meta.url));
const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const w = instance.exports;

const dev = new MockDevice();
const cam = new ZeluxCamera(dev);
const info = await cam.open();
console.log('info:', JSON.stringify({ ...info, applied: undefined }));
console.log('applied:', JSON.stringify(info.applied));

new Float32Array(w.memory.buffer, w.bw_matrix_ptr(), 9).set(info.matrix);
w.bw_configure(info.width, info.height, info.bitDepth, info.pixelWUm, info.pixelHUm, info.cfa);

const frames = [];
await cam.start((f) => frames.push(f));
const t0 = Date.now();
while (frames.length < 5 && Date.now() - t0 < 5000) await new Promise((r) => setTimeout(r, 10));
await cam.stop();
console.log('commands sent:', dev.log.join(' | '));
console.log('frames:', frames.map((f) => f.frame).join(','), 'stats:', JSON.stringify(cam.stats));

let failures = 0;
const td = new TextDecoder();
for (const f of frames) {
  const ref = `${base}.frame${f.frame}.u16`;
  const src = new Uint8Array(f.stream);
  const p = w.bw_stream_ptr(src.length);
  new Uint8Array(w.memory.buffer).set(src, p);
  if (!w.bw_decode(f.calibrationLines)) throw new Error('decode failed');
  // Copy now: processing may grow WebAssembly memory, which detaches views of the old buffer.
  const got = new Uint16Array(w.memory.buffer, w.bw_frame_ptr(), info.width * info.height).slice();
  const n = w.bw_process(1, 1);
  const r = JSON.parse(td.decode(new Uint8Array(w.memory.buffer, w.bw_json_ptr(), n)));
  if (!existsSync(ref)) {
    console.log(`frame ${f.frame}: no SDK reference`);
    continue;
  }
  const want = new Uint16Array(readFileSync(ref).buffer.slice(0));
  let diff = 0;
  for (let i = 0; i < want.length; i++) if (got[i] !== want[i]) diff++;
  if (diff) failures++;
  console.log(`frame ${f.frame}: ${diff === 0 ? 'identical to the SDK frame' : `${diff} pixels differ`}; ` +
    `analysis: detected=${r.detected} peak=${r.peak} background=${r.background?.toFixed(1)} ${r.no_beam ?? ''}`);
}
await cam.close();
if (frames.length === 0 || failures) {
  console.error('FAILED');
  process.exit(1);
}
console.log('OK');
