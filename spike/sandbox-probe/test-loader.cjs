const { chromium } = require('playwright'); const fs = require('fs');
(async () => {
  const b = await chromium.launch(); const p = await b.newPage();
  const errs = []; p.on('pageerror', e => errs.push('PAGEERROR ' + e.message.slice(0, 200)));
  p.on('console', m => { if (m.type() === 'error') errs.push(m.text().slice(0, 160)); });
  const csp = "default-src 'none'; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob: data:; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; font-src 'self'; media-src 'self' data:; frame-src 'none'; object-src 'none';";
  const html = fs.readFileSync('loader.built.fake.html', 'utf8').replace('<head>', `<head><meta http-equiv="Content-Security-Policy" content="${csp}">`);
  await p.setContent(`<iframe id=f sandbox="allow-scripts allow-same-origin" style="width:1000px;height:900px"></iframe>`);
  const full = fs.readFileSync('pluto.inner.html', 'utf8');
  await p.evaluate(f => { window.__FULL = f; }, full);
  await p.$eval('#f', (f, s) => { f.srcdoc = s; }, html);
  await p.waitForTimeout(14000);
  console.log('report:', await p.evaluate(() => window.__REPORT));
  console.log([...new Set(errs)].filter(e => !/font|Failed to load resource/.test(e)).slice(0, 8).join('\n'));
  await p.screenshot({ path: 'loader.png' }); await b.close();
})();
