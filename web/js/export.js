// Downloads: PNG of the display, 16-bit TIFF of the raw frame, CSV of results and profiles, CSV log.
// Everything is built in the browser; files are saved through the browser's normal download.

export function download(name, blob) {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = name;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 10_000);
}

export function stamp(d = new Date()) {
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
}

/** Baseline TIFF, one strip, 16-bit grayscale, little-endian, with a description tag. */
export function tiff16(width, height, data, description = '') {
  const desc = new TextEncoder().encode(description + '\0');
  const entries = [
    [256, 4, 1, width], // ImageWidth
    [257, 4, 1, height], // ImageLength
    [258, 3, 1, 16], // BitsPerSample
    [259, 3, 1, 1], // Compression: none
    [262, 3, 1, 1], // Photometric: black is zero
    [270, 2, desc.length, 0], // ImageDescription (offset patched below)
    [273, 4, 1, 0], // StripOffsets (patched)
    [277, 3, 1, 1], // SamplesPerPixel
    [278, 4, 1, height], // RowsPerStrip
    [279, 4, 1, width * height * 2], // StripByteCounts
    [284, 3, 1, 1], // PlanarConfiguration: contiguous
  ];
  const ifdSize = 2 + entries.length * 12 + 4;
  const descOff = 8 + ifdSize;
  const dataOff = descOff + desc.length + (desc.length % 2);
  entries[5][3] = descOff;
  entries[6][3] = dataOff;
  const buf = new ArrayBuffer(dataOff + width * height * 2);
  const dv = new DataView(buf);
  dv.setUint16(0, 0x4949, true); // "II"
  dv.setUint16(2, 42, true);
  dv.setUint32(4, 8, true);
  dv.setUint16(8, entries.length, true);
  entries.forEach(([tag, type, count, value], i) => {
    const o = 10 + i * 12;
    dv.setUint16(o, tag, true);
    dv.setUint16(o + 2, type, true);
    dv.setUint32(o + 4, count, true);
    if (type === 3 && count === 1) dv.setUint16(o + 8, value, true);
    else dv.setUint32(o + 8, value, true);
  });
  dv.setUint32(10 + entries.length * 12, 0, true);
  new Uint8Array(buf, descOff, desc.length).set(desc);
  const px = new Uint16Array(buf, dataOff, width * height);
  px.set(data);
  return new Blob([buf], { type: 'image/tiff' });
}

const f = (v, d) => (v == null || !Number.isFinite(v) ? '' : v.toFixed(d));

/** Results and both profiles of one analysed frame, like the desktop app's results table. */
export function resultsCsv(r, meta) {
  const [pw, ph] = r.pixel_um;
  const u = r.unit;
  const rows = [
    ['# Beam Profiler (web)', new Date().toISOString()],
    ['# camera', meta.model ?? '', 'serial', meta.serial ?? '', 'bit_depth', meta.bitDepth ?? ''],
    ['# exposure_ms', f(meta.applied?.exposureMs, 3), 'gain', meta.applied?.gain ?? '', 'gain_dB', f(meta.applied?.gainDb, 2), 'black_level', meta.applied?.blackLevel ?? ''],
    ['# analysis_pixel', u, 'width_um', f(pw, 4), 'height_um', f(ph, 4)],
    ['# frame', meta.frame ?? '', 'beam_detected', r.detected ? 1 : 0, 'reason', (r.no_beam ?? '').replaceAll(',', ';')],
    [],
    ['quantity', 'x', 'y', 'unit'],
    ['D_1/e2 (4 sigma)', f(r.dx_um, 3), f(r.dy_um, 3), 'um'],
    ['w_1/e2 radius (2 sigma)', f(r.dx_um / 2, 3), f(r.dy_um / 2, 3), 'um'],
    ['FWHM (2.3548 sigma)', f(r.x.fwhm * pw, 3), f(r.y.fwhm * ph, 3), 'um'],
    ['sigma (fit)', f(r.x.sigma, 4), f(r.y.sigma, 4), u],
    ['sigma (2nd moment)', f(r.x.sigma2, 4), f(r.y.sigma2, 4), u],
    ['center', f(r.x.center, 3), f(r.y.center, 3), u],
    ['fit amplitude A', f(r.x.amplitude, 3), f(r.y.amplitude, 3), 'counts (summed)'],
    ['fit offset B', f(r.x.offset, 3), f(r.y.offset, 3), 'counts (summed)'],
    ['fit S/N', f(r.x.snr, 2), f(r.y.snr, 2), ''],
    ['fit R2', f(r.x.r2, 4), f(r.y.r2, 4), ''],
    ['peak', r.peak, '', 'counts'],
    ['background', f(r.background, 2), '', 'counts'],
    ['saturated', r.saturated ? 1 : 0, '', ''],
    ['roi (row0, col0, rows, cols)', r.roi.join(' '), '', u],
    [],
  ];
  const profile = (name, a) => {
    rows.push([`${name}_${u}`, `${name}_profile`, `${name}_fit`]);
    a.profile.forEach((v, i) => rows.push([a.x0 + i, f(v, 3), f(a.fit[i], 3)]));
    rows.push([]);
  };
  profile('x', r.x);
  profile('y', r.y);
  return rows.map((c) => c.join(',')).join('\n') + '\n';
}

export function logHeader(unit) {
  const u = unit;
  return `t_s,frame,beam,sigma_x_${u},sigma_y_${u},sigma2_x_${u},sigma2_y_${u},Dx_um,Dy_um,center_x_${u},center_y_${u},peak,saturated\n`;
}

export function logRow(t, frame, r) {
  return [f(t, 3), frame, r.detected ? 1 : 0, f(r.x.sigma, 4), f(r.y.sigma, 4), f(r.x.sigma2, 4), f(r.y.sigma2, 4),
    f(r.dx_um, 3), f(r.dy_um, 3), f(r.x.center, 3), f(r.y.center, 3), r.peak, r.saturated ? 1 : 0].join(',') + '\n';
}
