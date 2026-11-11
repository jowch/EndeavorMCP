const { chromium } = require('playwright'); const fs = require('fs');
(async () => {
  const b = await chromium.launch(); const p = await b.newPage();
  const csp = "default-src 'none'; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob: data:; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; font-src 'self'; media-src 'self' data:; frame-src 'none'; object-src 'none';";
  const html = fs.readFileSync('loader.built.fake.html', 'utf8').replace('<head>', `<head><meta http-equiv="Content-Security-Policy" content="${csp}">`);
  await p.setContent(`<iframe id=f sandbox="allow-scripts allow-same-origin" style="width:700px;height:900px"></iframe>`);
  await p.evaluate(f => { window.__FULL = f; }, fs.readFileSync(process.env.PARTS, 'utf8'));
  const t0 = Date.now();
  await p.$eval('#f', (f, s) => { f.srcdoc = s; }, html);
  const seen = {};
  for (let i = 0; i < 300; i++) {
    await p.waitForTimeout(50);
    const fr = p.frames()[1]; if (!fr) continue;
    let r; try { r = await fr.evaluate(() => ({ cells: document.querySelectorAll('pluto-cell').length, svg: document.querySelectorAll('pluto-output svg polyline').length, canvas: [...document.querySelectorAll('pluto-output canvas')].filter(c => { try { return c.getContext('2d').getImageData(6, 10, 1, 1).data[0] === 255; } catch (e) { return false; } }).length })); } catch (e) { continue; }
    for (const [k, v] of Object.entries(r)) { if (v && !seen[k + '_first']) seen[k + '_first'] = Date.now() - t0; if (v && v !== seen[k]) { seen[k] = v; seen[k + '_last'] = Date.now() - t0; } }
  }
  console.log(JSON.stringify(seen));
  const m = await p.frames()[1].evaluate(() => performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1e6) + ' MB heap' : '');
  console.log(m);
  await b.close();
})();
