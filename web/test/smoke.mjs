// Smoke test of the page in a real (headless) Chromium browser over the DevTools protocol: opens the simulator,
// waits, reads the results the page shows, checks there were no errors, and saves a screenshot.
//
//   node web/test/smoke.mjs <browser exe> <page url> <screenshot.png> [seconds]
// e.g. node web/test/smoke.mjs "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" \
//        "http://127.0.0.1:8000/index.html?sim=mono" shot.png 6

import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [exe, url, shot, secs = '6'] = process.argv.slice(2);
if (!exe || !url || !shot) {
  console.error('usage: node smoke.mjs <browser exe> <url> <screenshot.png> [seconds]');
  process.exit(2);
}
const profile = mkdtempSync(join(tmpdir(), 'beam-smoke-'));
const port = 9300 + Math.floor(Math.random() * 500);
const browser = spawn(exe, [
  '--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, '--window-size=1400,1000',
  '--no-first-run', '--disable-extensions', 'about:blank',
], { stdio: 'ignore' });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let ws;
try {
  let target;
  for (let i = 0; i < 50 && !target; i++) {
    await sleep(200);
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      target = list.find((t) => t.type === 'page');
    } catch {}
  }
  if (!target) throw new Error('browser did not start');
  ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const waiting = new Map();
  const problems = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id && waiting.has(m.id)) {
      waiting.get(m.id)(m);
      waiting.delete(m.id);
    } else if (m.method === 'Runtime.exceptionThrown') {
      problems.push(`exception: ${m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text}`);
    } else if (m.method === 'Runtime.consoleAPICalled' && ['error', 'warning'].includes(m.params.type)) {
      problems.push(`console.${m.params.type}: ${m.params.args.map((a) => a.value ?? a.description).join(' ')}`);
    } else if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error') {
      problems.push(`log: ${m.params.entry.text} ${m.params.entry.url ?? ''}`);
    }
  };
  const send = (method, params = {}) => new Promise((r) => {
    const i = ++id;
    waiting.set(i, r);
    ws.send(JSON.stringify({ id: i, method, params }));
  });
  await send('Runtime.enable');
  await send('Log.enable');
  await send('Page.enable');
  await send('Page.navigate', { url });
  await sleep(Number(secs) * 1000);
  const ev = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const t = (id) => document.getElementById(id)?.textContent.trim();
      const msg = document.getElementById('message');
      return { status: t('status'), dx: t('r-dx'), dy: t('r-dy'), state: t('r-state'), center: t('r-center'),
               fps: t('r-fps'), zoom: t('zoom-text'), message: msg.hidden ? null : msg.textContent,
               placeholderHidden: document.getElementById('placeholder').hidden };
    })()`,
  });
  const page = ev.result.result.value;
  const png = await send('Page.captureScreenshot', { format: 'png' });
  writeFileSync(shot, Buffer.from(png.result.data, 'base64'));
  console.log(JSON.stringify(page, null, 1));
  if (problems.length) console.log('problems:\n  ' + problems.join('\n  '));
  const ok = page.placeholderHidden && !page.message && problems.length === 0 && page.dx !== '–';
  console.log(ok ? 'OK' : 'FAILED');
  process.exitCode = ok ? 0 : 1;
} finally {
  ws?.close();
  browser.kill();
  await sleep(500);
  try { rmSync(profile, { recursive: true, force: true }); } catch {}
}
