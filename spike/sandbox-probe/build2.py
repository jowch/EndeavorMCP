import sys
lib = open(sys.argv[1] if len(sys.argv) > 1 else 'applib.js').read().replace('</script', '<\\/script')
suffix = sys.argv[2] if len(sys.argv) > 2 else ''
open(f'loader.built{suffix}.html', 'w').write(open('loader.html').read().replace('/*APPLIB*/', lib))
tail = '''<script type="module">/*APPLIB*/
const { App } = window.McpAppLib;
const app = new App({ name: "Endeavor CDN probe", version: "0.0.1" }, { availableDisplayModes: ["inline", "fullscreen"] });
await app.connect();
setTimeout(async () => {
  const c = document.querySelector("pluto-output canvas");
  const r = { cells: document.querySelectorAll("pluto-cell").length, canvas: c ? Array.from(c.getContext("2d").getImageData(15, 20, 1, 1).data).join(",") : "none", editor: document.querySelector("pluto-editor")?.className };
  await app.callServerTool({ name: "probe_report", arguments: { report: "cdn " + JSON.stringify(r) } });
}, 10000);
</script>'''.replace('/*APPLIB*/', lib)
p = open('../nb/cdn.inl.html').read()
open(f'cdn.built{suffix}.html', 'w').write(p.replace('</body>', tail + '</body>', 1))
import shutil; shutil.copy('../nb/inlined.html', 'pluto.inner.html')
