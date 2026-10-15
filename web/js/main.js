// Page controller: picks the frame source (WebUSB camera or simulator), feeds frames to the processing worker,
// and shows what comes back. Like the desktop app, frames are dropped rather than queued when processing is
// busy, and the display (auto-zoom, images) is refreshed at most 10 times a second.

import { ZeluxCamera, supported, pairedCameras, requestCamera, CameraError } from './cs165.js';
import { drawProfile, fitCanvas } from './plot.js';
import { download, stamp, tiff16, resultsCsv, logHeader, logRow } from './export.js';

const $ = (id) => document.getElementById(id);
const DISPLAY_MS = 100;

const SIM_INFO = (color) => ({
  model: color ? 'Simulated CS165CU' : 'Simulated CS165MU',
  serial: color ? 'SIM-0002' : 'SIM-0001',
  firmware: '',
  width: 1440,
  height: 1080,
  bitDepth: 10,
  pixelWUm: 3.45,
  pixelHUm: 3.45,
  color,
  cfa: color ? 0 : -1,
  // The simulator's daylight white balance; identity colour correction.
  matrix: color ? [1.6, 0, 0, 0, 1, 0, 0, 0, 1.9] : [1, 0, 0, 0, 1, 0, 0, 0, 1],
  exposureRangeUs: [40, 26843000],
  gainRange: [0, 480],
  blackLevelRange: [0, 100],
  ledSupported: true,
  applied: { exposureMs: 5, gain: 0, gainDb: 0, blackLevel: 12, ledOn: false },
});

const state = {
  source: null, // 'camera' | 'sim'
  camera: null,
  info: null,
  workerBusy: false,
  workerReady: null,
  lastDisplay: 0,
  dropped: 0,
  frames: 0,
  fps: 0,
  fpsMark: null,
  frameNo: 0,
  result: null,
  paused: false,
  simTimer: null,
  simT0: 0,
  simBlocked: false,
  log: null,
  connecting: false,
};

// ---- worker ------------------------------------------------------------------------------------------------------

const worker = new Worker(new URL('./worker.js', import.meta.url));
state.workerReady = new Promise((resolve, reject) => {
  worker.onmessage = (e) => {
    if (e.data.type === 'ready') resolve();
    else if (e.data.type === 'error') reject(new Error(e.data.message));
  };
});
worker.postMessage({ type: 'init', wasmUrl: new URL('../wasm/beam_wasm.wasm', import.meta.url).href });
state.workerReady.then(() => {
  worker.onmessage = onWorkerMessage;
}).catch((e) => showMessage(`Could not load the analysis module: ${e.message}`, true));

let rawWaiter = null;

function onWorkerMessage(e) {
  const m = e.data;
  if (m.type === 'result') {
    state.workerBusy = false;
    state.result = m.result;
    showResult(m.result);
    if (m.zoom) drawImages(m.zoom, m.full, m.result);
    if (state.log && !state.paused) state.log.rows.push(logRow((performance.now() - state.log.t0) / 1000, state.frameNo, m.result));
  } else if (m.type === 'raw') {
    rawWaiter?.(m);
    rawWaiter = null;
  } else if (m.type === 'error') {
    state.workerBusy = false;
    showMessage(`Processing error (${m.during}): ${m.message}`, true);
  }
}

function wantDisplay() {
  const now = performance.now();
  if (now - state.lastDisplay >= DISPLAY_MS) {
    state.lastDisplay = now;
    return true;
  }
  return false;
}

function countFrame(frameNo) {
  state.frames++;
  state.frameNo = frameNo;
  const now = performance.now();
  if (!state.fpsMark) state.fpsMark = { t: now, n: state.frames };
  const dt = now - state.fpsMark.t;
  if (dt >= 1000) {
    state.fps = ((state.frames - state.fpsMark.n) * 1000) / dt;
    state.fpsMark = { t: now, n: state.frames };
  }
}

// ---- sources -----------------------------------------------------------------------------------------------------

async function configureWorker(info) {
  await state.workerReady;
  worker.postMessage({ type: 'configure', cfg: info });
  sendControls();
}

async function startSim(color) {
  await stopSource();
  const info = SIM_INFO(color);
  state.info = info;
  state.source = 'sim';
  await configureWorker(info);
  showInfo(info);
  setCameraInputs(info);
  setStatus('live', info.model);
  state.simT0 = performance.now();
  const tick = () => {
    if (state.source !== 'sim') return;
    const a = info.applied;
    // At most 30 frames per second, slower for long exposures.
    const period = Math.max(1000 / 30, a.exposureMs);
    state.simTimer = setTimeout(tick, period);
    if (state.paused || state.workerBusy) {
      if (!state.paused) state.dropped++;
      return;
    }
    countFrame(state.frames + 1);
    state.workerBusy = true;
    worker.postMessage({
      type: 'simulate',
      t: (performance.now() - state.simT0) / 1000,
      exposureMs: a.exposureMs,
      gainDb: a.gainDb,
      blackLevel: a.blackLevel,
      blocked: state.simBlocked,
      display: wantDisplay(),
    });
  };
  tick();
}

async function connectCamera(device) {
  if (state.connecting) return;
  state.connecting = true;
  await stopSource();
  setStatus('busy', 'Connecting…');
  const cam = new ZeluxCamera(device);
  try {
    const info = await cam.open();
    state.camera = cam;
    state.info = info;
    state.source = 'camera';
    await configureWorker(info);
    showInfo(info);
    setCameraInputs(info);
    cam.onError = (e) => {
      showMessage(`Camera stopped: ${e.message}`, true);
      stopSource();
    };
    await cam.start(onCameraFrame);
    setStatus('live', `${info.model} · ${info.serial}`);
    hideMessage();
  } catch (e) {
    await cam.close().catch(() => {});
    state.camera = null;
    state.source = null;
    setStatus('error', 'Not connected');
    showMessage(explainUsbError(e), true);
  } finally {
    state.connecting = false;
  }
}

function onCameraFrame(f) {
  if (state.source !== 'camera') return;
  countFrame(f.frame);
  if (state.paused) return;
  if (state.workerBusy) {
    state.dropped++;
    return;
  }
  state.workerBusy = true;
  worker.postMessage({ type: 'frame', stream: f.stream, calibrationLines: f.calibrationLines, display: wantDisplay() }, [f.stream]);
}

async function stopSource() {
  clearTimeout(state.simTimer);
  const cam = state.camera;
  state.source = null;
  state.camera = null;
  state.workerBusy = false;
  state.frames = 0;
  state.dropped = 0;
  state.fpsMark = null;
  state.fps = 0;
  if (cam) await cam.close().catch(() => {});
  setStatus('idle', 'No camera');
  updateButtons();
}

function explainUsbError(e) {
  const msg = String(e?.message ?? e);
  if (e?.name === 'NotFoundError') return 'No camera was chosen.';
  if (e?.name === 'SecurityError' || /Access denied|Unable to claim|claimInterface/i.test(msg)) {
    return 'The browser cannot open the camera. On Windows the camera must use the WinUSB driver instead of the ' +
      'Thorlabs driver, and ThorCam (or any other program using the camera) must be closed. See Setup help.';
  }
  if (e instanceof CameraError) return `Camera error: ${msg}`;
  return `Could not open the camera: ${msg}`;
}

// ---- controls ----------------------------------------------------------------------------------------------------

function numberOf(id, fallback) {
  const v = Number($(id).value);
  return Number.isFinite(v) && $(id).value !== '' ? v : fallback;
}

function plotAspect() {
  const c = $('zoom');
  return c.clientWidth > 0 && c.clientHeight > 0 ? c.clientWidth / c.clientHeight : 4 / 3;
}

function sendControls() {
  worker.postMessage({
    type: 'controls',
    c: {
      roiHalf: Math.max(0, Math.round(numberOf('a-roi', 150))),
      minContrast: numberOf('a-contrast', 30),
      minSnr: numberOf('a-snr', 8),
      zoomMode: Number($('d-mode').value),
      thresholdPct: numberOf('d-threshold', 13.5),
      maxZoom: numberOf('d-maxzoom', 16),
      margin: 0.5,
      palette: Number($('d-palette').value),
      channel: Number($('a-channel').value),
      colorView: $('d-color').checked,
      plotAspect: plotAspect(),
    },
  });
  if (state.paused && state.result && !state.workerBusy) {
    state.workerBusy = true;
    worker.postMessage({ type: 'reprocess', display: true });
  }
}

let applyTimer = null;
function scheduleApply() {
  clearTimeout(applyTimer);
  applyTimer = setTimeout(applySettings, 250);
}

async function applySettings() {
  const info = state.info;
  if (!info) return;
  const s = {
    exposureMs: Math.max(0.04, numberOf('c-exposure', 5)),
    gain: Math.round(numberOf('c-gain', 0) * 10),
    blackLevel: $('c-black').value === '' ? null : Math.round(numberOf('c-black', 0)),
    ledOn: $('c-led').checked,
  };
  try {
    if (state.source === 'camera') {
      await state.camera.apply(s);
    } else if (state.source === 'sim') {
      const clamp = (v, [lo, hi]) => Math.min(Math.max(v, lo), hi);
      const exp = clamp(Math.round(s.exposureMs * 1000), info.exposureRangeUs) / 1000;
      const gain = clamp(s.gain, info.gainRange);
      info.applied = { exposureMs: exp, gain, gainDb: gain / 10, blackLevel: s.blackLevel ?? info.applied.blackLevel, ledOn: s.ledOn };
    }
    showApplied(info.applied);
  } catch (e) {
    showMessage(`Settings not applied: ${e.message}`, true);
  }
}

function setCameraInputs(info) {
  const a = info.applied;
  $('c-exposure').value = +a.exposureMs.toFixed(3);
  $('c-exposure').min = info.exposureRangeUs[0] / 1000;
  $('c-exposure').max = info.exposureRangeUs[1] / 1000;
  $('c-gain').value = a.gainDb;
  $('c-gain').max = info.gainRange[1] / 10;
  $('c-gain').disabled = !(info.gainRange[1] > info.gainRange[0]);
  $('c-black').value = a.blackLevel;
  $('c-black').max = info.blackLevelRange[1];
  $('c-led').checked = a.ledOn;
  $('c-led').disabled = !info.ledSupported;
  $('a-channel-wrap').hidden = !info.color;
  $('d-color-wrap').hidden = !info.color;
  showApplied(a);
}

// ---- display -----------------------------------------------------------------------------------------------------

const fmt = (v, d = 1) => (v == null || !Number.isFinite(v) ? '–' : v.toFixed(d));

function showResult(r) {
  $('r-dx').textContent = fmt(r.dx_um);
  $('r-dy').textContent = fmt(r.dy_um);
  const st = $('r-state');
  if (r.detected && r.saturated) {
    st.textContent = 'Beam found, but saturated: reduce exposure or gain';
    st.className = 'state warn';
  } else if (r.detected) {
    st.textContent = 'Beam found';
    st.className = 'state ok';
  } else {
    st.textContent = r.no_beam ? r.no_beam.charAt(0).toUpperCase() + r.no_beam.slice(1) : 'No beam';
    st.className = 'state bad';
  }
  const [pw, ph] = r.pixel_um;
  $('r-center').textContent = r.detected ? `${fmt(r.x.center)}, ${fmt(r.y.center)} ${r.unit}` : '–';
  $('r-fwhm').textContent = r.detected ? `${fmt(r.x.fwhm * pw)} / ${fmt(r.y.fwhm * ph)} µm` : '–';
  $('r-d2').textContent = r.detected ? `${fmt(4 * r.x.sigma2 * pw)} / ${fmt(4 * r.y.sigma2 * ph)} µm` : '–';
  $('r-peak').textContent = `${r.peak} / ${fmt(r.background)} of ${r.full_scale}`;
  $('r-snr').textContent = `${fmt(r.x.snr)} / ${fmt(r.y.snr)}`;
  $('r-r2').textContent = `${fmt(r.x.r2, 3)} / ${fmt(r.y.r2, 3)}`;
  $('r-fps').textContent = `${fmt(state.fps)} fps${state.dropped ? ` · ${state.dropped} skipped` : ''}`;
  $('dark-status').textContent = r.dark_status;
  if (r.zoom_text) $('zoom-text').textContent = r.zoom_text;
  $('frame-text').textContent = state.paused ? 'Paused' : `Frame ${state.frameNo}`;
  drawProfile($('plot-x'), r.x);
  drawProfile($('plot-y'), r.y);
}

const offscreen = document.createElement('canvas');
function blit(canvas, pic) {
  offscreen.width = pic.width;
  offscreen.height = pic.height;
  offscreen.getContext('2d').putImageData(new ImageData(pic.rgba, pic.width, pic.height), 0, 0);
  const { ctx, w, h } = fitCanvas(canvas);
  ctx.imageSmoothingEnabled = false;
  ctx.clearRect(0, 0, w, h);
  // The window has the canvas' aspect ratio, so it fills the canvas with square pixels.
  ctx.drawImage(offscreen, 0, 0, w, h);
}

function drawImages(zoom, full, r) {
  $('placeholder').hidden = true;
  blit($('zoom'), zoom);
  blit($('full'), full);
  const [row0, col0, rows, cols] = r.window;
  const [iw, ih] = r.size;

  // Zoom overlay: the fitted 1/e² ellipse (2 sigma radii) and its centre.
  const ov = fitCanvas($('overlay'));
  ov.ctx.clearRect(0, 0, ov.w, ov.h);
  const ell = r.detected && $('d-ellipse').checked && r.x.center != null && r.y.center != null;
  const dpr = ov.dpr;
  if (ell) {
    const sx = ov.w / cols, sy = ov.h / rows;
    const cx = (r.x.center + 0.5 - col0) * sx, cy = (r.y.center + 0.5 - row0) * sy;
    ov.ctx.strokeStyle = r.saturated ? '#ff4040' : '#3ee07a';
    ov.ctx.lineWidth = 1.5 * dpr;
    ov.ctx.beginPath();
    ov.ctx.ellipse(cx, cy, 2 * r.x.sigma * sx, 2 * r.y.sigma * sy, 0, 0, 2 * Math.PI);
    ov.ctx.stroke();
    ov.ctx.beginPath();
    ov.ctx.moveTo(cx - 6 * dpr, cy);
    ov.ctx.lineTo(cx + 6 * dpr, cy);
    ov.ctx.moveTo(cx, cy - 6 * dpr);
    ov.ctx.lineTo(cx, cy + 6 * dpr);
    ov.ctx.stroke();
  }

  // Overview overlay: where the zoom window is.
  const fo = fitCanvas($('full-overlay'));
  fo.ctx.clearRect(0, 0, fo.w, fo.h);
  if (rows < ih || cols < iw) {
    fo.ctx.strokeStyle = '#f0a030';
    fo.ctx.lineWidth = 1.5 * fo.dpr;
    fo.ctx.strokeRect((col0 / iw) * fo.w, (row0 / ih) * fo.h, (cols / iw) * fo.w, (rows / ih) * fo.h);
  }
}

function showInfo(info) {
  const rows = [
    ['Model', info.model],
    ['Serial', info.serial],
    info.firmware ? ['Firmware', info.firmware] : null,
    ['Sensor', `${info.width} × ${info.height}, ${info.bitDepth}-bit, ${info.color ? 'colour (Bayer)' : 'mono'}`],
    ['Pixel', `${info.pixelWUm} × ${info.pixelHUm} µm${info.color ? ' (analysed per 2 × 2 cell)' : ''}`],
  ].filter(Boolean);
  $('info').innerHTML = rows.map(([k, v]) => `<tr><th>${k}</th><td>${escapeHtml(String(v))}</td></tr>`).join('');
  updateButtons();
}

function showApplied(a) {
  $('applied').textContent = `Applied: ${fmt(a.exposureMs, 3)} ms, ${fmt(a.gainDb, 1)} dB, black ${a.blackLevel}${a.ledOn ? ', LED on' : ''}`;
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

function setStatus(kind, text) {
  const s = $('status');
  s.dataset.state = kind;
  s.textContent = text;
}

function showMessage(text, error = false) {
  const m = $('message');
  m.textContent = text;
  m.className = error ? 'banner error' : 'banner';
  m.hidden = false;
}

function hideMessage() {
  $('message').hidden = true;
}

function updateButtons() {
  const on = state.source != null;
  $('btn-stop').disabled = !on;
  $('btn-sim-mono').classList.toggle('active', state.source === 'sim' && !state.info?.color);
  $('btn-sim-color').classList.toggle('active', state.source === 'sim' && !!state.info?.color);
  for (const id of ['b-pause', 'b-png', 'b-tiff', 'b-csv', 'b-log', 'b-dark', 'b-dark-clear']) $(id).disabled = !on;
  $('b-pause').textContent = state.paused ? 'Play' : 'Pause';
}

// ---- saving ------------------------------------------------------------------------------------------------------

function baseName() {
  return `beam_${stamp()}`;
}

function savePng() {
  $('zoom').toBlob((b) => b && download(`${baseName()}.png`, b), 'image/png');
}

async function saveTiff() {
  const raw = await new Promise((resolve) => {
    rawWaiter = resolve;
    worker.postMessage({ type: 'raw' });
  });
  const i = state.info, a = i.applied;
  const desc = `${i.model} ${i.serial}; ${i.width}x${i.height} ${i.bitDepth}-bit; exposure ${a.exposureMs} ms; gain ${a.gainDb} dB; black ${a.blackLevel}; frame ${state.frameNo}`;
  download(`${baseName()}_raw.tif`, tiff16(raw.width, raw.height, raw.data, desc));
  $('save-status').textContent = 'Raw frame saved (16-bit counts, unprocessed).';
}

function saveCsv() {
  if (!state.result) return;
  const i = state.info;
  const csv = resultsCsv(state.result, { model: i.model, serial: i.serial, bitDepth: i.bitDepth, applied: i.applied, frame: state.frameNo });
  download(`${baseName()}.csv`, new Blob([csv], { type: 'text/csv' }));
}

function toggleLog() {
  if (state.log) {
    const l = state.log;
    state.log = null;
    download(`beam_log_${l.stamp}.csv`, new Blob([logHeader(l.unit), ...l.rows], { type: 'text/csv' }));
    $('b-log').textContent = 'Start log';
    $('save-status').textContent = `Log saved: ${l.rows.length} frames.`;
  } else {
    state.log = { t0: performance.now(), rows: [], stamp: stamp(), unit: state.info?.color ? 'cell' : 'px' };
    $('b-log').textContent = 'Stop log and save';
    $('save-status').textContent = 'Logging every analysed frame…';
  }
}

// ---- wiring ------------------------------------------------------------------------------------------------------

function wire() {
  $('btn-connect').addEventListener('click', async () => {
    try {
      const dev = await requestCamera();
      await connectCamera(dev);
    } catch (e) {
      if (e?.name !== 'NotFoundError') showMessage(explainUsbError(e), true);
    }
    updateButtons();
  });
  $('btn-sim-mono').addEventListener('click', () => startSim(false).then(updateButtons));
  $('btn-sim-color').addEventListener('click', () => startSim(true).then(updateButtons));
  $('btn-stop').addEventListener('click', () => stopSource());

  for (const id of ['c-exposure', 'c-gain', 'c-black', 'c-led']) $(id).addEventListener('change', scheduleApply);
  for (const id of ['a-roi', 'a-contrast', 'a-snr', 'a-channel', 'd-mode', 'd-palette', 'd-threshold', 'd-maxzoom', 'd-color']) {
    $(id).addEventListener('change', sendControls);
  }
  $('d-ellipse').addEventListener('change', () => state.paused && sendControls());

  $('b-dark').addEventListener('click', () => {
    if (state.paused) return showMessage('Press Play first: a dark frame is recorded from live frames.');
    if (state.source === 'sim') {
      // The simulator's shutter: block the beam while the dark frame is recorded.
      state.simBlocked = true;
      setTimeout(() => (state.simBlocked = false), 10 * Math.max(1000 / 30, state.info.applied.exposureMs) + 800);
    }
    worker.postMessage({ type: 'dark', frames: 10 });
  });
  $('b-dark-clear').addEventListener('click', () => worker.postMessage({ type: 'clearDark' }));
  $('b-pause').addEventListener('click', () => {
    state.paused = !state.paused;
    updateButtons();
    $('frame-text').textContent = state.paused ? 'Paused' : '';
  });
  $('b-png').addEventListener('click', savePng);
  $('b-tiff').addEventListener('click', saveTiff);
  $('b-csv').addEventListener('click', saveCsv);
  $('b-log').addEventListener('click', toggleLog);

  // Keep square pixels when the layout changes size.
  new ResizeObserver(() => state.source && sendControls()).observe($('zoom'));
}

async function autoDetect() {
  if (!supported()) {
    $('unsupported').hidden = false;
    $('btn-connect').disabled = true;
    return;
  }
  // A camera this site may already use: connect without asking. Plugging one in later connects it too.
  navigator.usb.addEventListener('connect', async (e) => {
    const [dev] = (await pairedCameras()).filter((d) => d === e.device);
    if (dev && state.source !== 'camera') connectCamera(dev).then(updateButtons);
  });
  navigator.usb.addEventListener('disconnect', (e) => {
    if (state.camera && e.device === state.camera.device) {
      showMessage('The camera was unplugged.');
      stopSource();
    }
  });
  const paired = await pairedCameras();
  if (paired.length) await connectCamera(paired[0]);
}

wire();
updateButtons();
// `?sim=mono` or `?sim=color` starts the simulator at once (demo links, tests); otherwise look for a camera.
const simParam = new URLSearchParams(location.search).get('sim');
if (simParam) {
  if (!supported()) $('unsupported').hidden = false;
  startSim(simParam === 'color').then(updateButtons);
} else {
  autoDetect().then(updateButtons);
}
