// Small canvas plots: a profile (points) with its Gaussian fit (line), x in sensor pixels.

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** Sizes the canvas backing store to its displayed size (device pixels) and returns the 2D context. */
export function fitCanvas(canvas) {
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
  const h = Math.max(1, Math.round(canvas.clientHeight * dpr));
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  return { ctx: canvas.getContext('2d'), w, h, dpr };
}

function niceStep(range, target) {
  const raw = range / target;
  const p = 10 ** Math.floor(Math.log10(raw));
  return [1, 2, 5, 10].map((m) => m * p).find((s) => s >= raw) ?? raw;
}

/**
 * @param {HTMLCanvasElement} canvas
 * @param {{x0:number, profile:number[], fit:number[], center:number|null, sigma:number|null}} axis
 */
export function drawProfile(canvas, axis) {
  const { ctx, w, h, dpr } = fitCanvas(canvas);
  ctx.clearRect(0, 0, w, h);
  const prof = axis?.profile ?? [];
  if (prof.length < 2) return;
  const fit = axis.fit ?? [];
  const pad = { l: 44 * dpr, r: 8 * dpr, t: 8 * dpr, b: 20 * dpr };
  const all = prof.concat(fit).filter((v) => v != null && Number.isFinite(v));
  let lo = Math.min(...all), hi = Math.max(...all);
  if (hi <= lo) hi = lo + 1;
  const span = hi - lo;
  lo -= span * 0.05;
  hi += span * 0.05;
  const x0 = axis.x0, x1 = axis.x0 + prof.length - 1;
  const X = (x) => pad.l + ((x - x0) / Math.max(1, x1 - x0)) * (w - pad.l - pad.r);
  const Y = (y) => h - pad.b - ((y - lo) / (hi - lo)) * (h - pad.t - pad.b);

  // Grid and labels
  ctx.strokeStyle = cssVar('--plot-grid');
  ctx.fillStyle = cssVar('--muted');
  ctx.lineWidth = 1;
  ctx.font = `${10 * dpr}px system-ui, sans-serif`;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'top';
  const sx = niceStep(x1 - x0, 6);
  for (let x = Math.ceil(x0 / sx) * sx; x <= x1; x += sx) {
    ctx.beginPath();
    ctx.moveTo(X(x), pad.t);
    ctx.lineTo(X(x), h - pad.b);
    ctx.stroke();
    ctx.fillText(String(x), X(x), h - pad.b + 4 * dpr);
  }
  ctx.textAlign = 'right';
  ctx.textBaseline = 'middle';
  const sy = niceStep(hi - lo, 4);
  for (let y = Math.ceil(lo / sy) * sy; y <= hi; y += sy) {
    ctx.beginPath();
    ctx.moveTo(pad.l, Y(y));
    ctx.lineTo(w - pad.r, Y(y));
    ctx.stroke();
    ctx.fillText(y >= 1e4 ? y.toExponential(1) : String(Math.round(y)), pad.l - 4 * dpr, Y(y));
  }

  // 1/e² band of the fit
  if (axis.center != null && axis.sigma != null) {
    ctx.fillStyle = cssVar('--plot-fit') + '22';
    const a = X(Math.max(x0, axis.center - 2 * axis.sigma)), b = X(Math.min(x1, axis.center + 2 * axis.sigma));
    ctx.fillRect(Math.min(a, b), pad.t, Math.abs(b - a), h - pad.t - pad.b);
  }

  // Profile points
  ctx.fillStyle = cssVar('--plot-profile');
  const r = Math.max(1, 1.3 * dpr);
  prof.forEach((v, i) => {
    if (v == null) return;
    ctx.fillRect(X(x0 + i) - r, Y(v) - r, 2 * r, 2 * r);
  });

  // Fit
  if (fit.length === prof.length) {
    ctx.strokeStyle = cssVar('--plot-fit');
    ctx.lineWidth = 2 * dpr;
    ctx.beginPath();
    fit.forEach((v, i) => (i ? ctx.lineTo(X(x0 + i), Y(v)) : ctx.moveTo(X(x0 + i), Y(v))));
    ctx.stroke();
  }
}
