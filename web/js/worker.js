// Processing worker: owns the WebAssembly build of the `beam` analysis (beam-wasm) and does everything per frame
// off the page's thread, like the desktop app's processing thread.
//
// Messages in:  init {wasmUrl} | configure {cfg} | controls {c} | frame {stream, calibrationLines, display}
//               | simulate {t, exposureMs, gainDb, blackLevel, blocked, display} | reprocess {display}
//               | dark {frames} | clearDark | raw
// Messages out: ready | result {result, zoom, full} | raw {width, height, data} | error {message}

let wasm = null;
let mem = () => new Uint8Array(wasm.memory.buffer);
let cfg = null;

const td = new TextDecoder();

function picture(which) {
  const w = wasm.bw_pic_width(which), h = wasm.bw_pic_height(which);
  if (!w || !h) return null;
  const p = wasm.bw_pic_ptr(which);
  // Copy out of WebAssembly memory so the buffer can be transferred.
  const rgba = new Uint8ClampedArray(wasm.memory.buffer, p, w * h * 4).slice();
  return { width: w, height: h, rgba };
}

function process(live, display) {
  const n = wasm.bw_process(live ? 1 : 0, display ? 1 : 0);
  const result = JSON.parse(td.decode(mem().subarray(wasm.bw_json_ptr(), wasm.bw_json_ptr() + n)));
  const zoom = display ? picture(0) : null;
  const full = display ? picture(1) : null;
  const transfer = [zoom?.rgba.buffer, full?.rgba.buffer].filter(Boolean);
  postMessage({ type: 'result', result, zoom, full }, transfer);
}

// Messages are handled one at a time, in order: 'init' is asynchronous, and anything sent meanwhile waits for it.
let chain = Promise.resolve();
onmessage = (e) => {
  chain = chain.then(() => handle(e.data));
};

async function handle(m) {
  try {
    switch (m.type) {
      case 'init': {
        const { instance } = await WebAssembly.instantiateStreaming(fetch(m.wasmUrl), {});
        wasm = instance.exports;
        postMessage({ type: 'ready' });
        break;
      }
      case 'configure': {
        cfg = m.cfg;
        new Float32Array(wasm.memory.buffer, wasm.bw_matrix_ptr(), 9).set(cfg.matrix ?? [1, 0, 0, 0, 1, 0, 0, 0, 1]);
        wasm.bw_configure(cfg.width, cfg.height, cfg.bitDepth, cfg.pixelWUm, cfg.pixelHUm, cfg.cfa ?? -1);
        break;
      }
      case 'controls': {
        const c = m.c;
        wasm.bw_set_controls(c.roiHalf, c.minContrast, c.minSnr, c.zoomMode, c.thresholdPct, c.maxZoom, c.margin, c.palette, c.channel, c.colorView ? 1 : 0, c.plotAspect);
        break;
      }
      case 'frame': {
        const src = new Uint8Array(m.stream);
        const p = wasm.bw_stream_ptr(src.length);
        mem().set(src, p);
        if (!wasm.bw_decode(m.calibrationLines)) throw new Error('Frame data too short for the image size');
        process(true, m.display);
        break;
      }
      case 'simulate':
        wasm.bw_simulate(m.t, m.exposureMs, m.gainDb, m.blackLevel, m.blocked ? 1 : 0);
        process(true, m.display);
        break;
      case 'reprocess':
        process(false, m.display);
        break;
      case 'dark':
        wasm.bw_record_dark(m.frames);
        break;
      case 'clearDark':
        wasm.bw_clear_dark();
        break;
      case 'raw': {
        const data = new Uint16Array(wasm.memory.buffer, wasm.bw_frame_ptr(), cfg.width * cfg.height).slice();
        postMessage({ type: 'raw', width: cfg.width, height: cfg.height, data }, [data.buffer]);
        break;
      }
    }
  } catch (err) {
    postMessage({ type: 'error', message: String(err?.message ?? err), during: m.type });
  }
}
