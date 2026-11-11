const { chromium } = require('playwright'); const fs = require('fs'); const { spawn } = require('child_process');
const srv = spawn('python3', ['-I', 'server.py'], { cwd: __dirname, stdio: ['pipe', 'pipe', 'inherit'] });
let nextId = 1; const pending = new Map(); let buf = '';
srv.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { const line = buf.slice(0, i); buf = buf.slice(i + 1); const m = JSON.parse(line); pending.get(m.id)?.(m); pending.delete(m.id); } });
const call = (method, params) => new Promise(res => { const id = nextId++; pending.set(id, res); srv.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n'); });
(async () => {
  await call('initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'test', version: '0' } });
  const b = await chromium.launch(); const p = await b.newPage();
  const errs = []; p.on('pageerror', e => errs.push('PAGEERROR ' + e.message.slice(0, 200)));
  p.on('console', m => { if (!/font/.test(m.text())) errs.push(m.text().slice(0, 160)); });
  await p.exposeFunction('__mcpCall', async (name, args) => { const r = await call('tools/call', { name, arguments: JSON.parse(args) }); return JSON.stringify(r.result ?? { error: r.error }); });
  const csp = "default-src 'none'; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob: data:; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; font-src 'self'; media-src 'self' data:; frame-src 'none'; object-src 'none';";
  const html = fs.readFileSync('live.built.fake.html', 'utf8').replace('<head>', `<head><meta http-equiv="Content-Security-Policy" content="${csp}">`);
  await p.setContent(`<iframe id=f sandbox="allow-scripts allow-same-origin" style="width:1000px;height:900px"></iframe>`);
  await p.$eval('#f', (f, s) => { f.srcdoc = s; }, html);
  await p.waitForTimeout(20000);
  const fr = p.frames()[1];
  console.log('stats0:', JSON.stringify(await fr.evaluate(() => ({s: window.__relayStats, cls: document.querySelector('pluto-editor')?.className, boot: document.body.innerText.slice(0,200)})))); console.log(errs.slice(0,15).join('\n'));
  const before = await fr.evaluate(() => [...document.querySelectorAll('pluto-output')].map(o => o.innerText.slice(0, 40)));
  console.log('outputs:', JSON.stringify(before));
  // move the slider: set value and fire input, Pluto sends the bond to Julia
  await fr.evaluate(() => { const r = document.querySelector('input[type=range]'); r.value = 12; r.dispatchEvent(new Event('input', { bubbles: true })); });
  await p.waitForTimeout(6000);
  const after = await fr.evaluate(() => [...document.querySelectorAll('pluto-output')].map(o => o.innerText.slice(0, 40)));
  console.log('after slider:', JSON.stringify(after));
  console.log('stats:', JSON.stringify(await fr.evaluate(() => window.__relayStats)));
  console.log([...new Set(errs)].filter(e => !/font|Failed to load resource|stats.plutojl|img-src/.test(e)).slice(0, 10).join('\n'));
  await p.screenshot({ path: 'live.png' }); await b.close(); srv.kill(); process.exit(0);
})();
