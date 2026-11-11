import sys
lib = open('applib.js').read()
v = open('view.html').read().replace('/*APPLIB*/', lib.replace('</script', '<\\/script'))
open('view.built.html', 'w').write(v)
p = open('../nb/inlined.html').read()
tail = '''<script type="module">/*APPLIB*/
const { App } = window.McpAppLib;
const app = new App({ name: "Endeavor Pluto probe", version: "0.0.1" }, { availableDisplayModes: ["inline", "fullscreen"] });
await app.connect();
setTimeout(async () => {
  const c = document.querySelector("pluto-output canvas");
  const r = { cells: document.querySelectorAll("pluto-cell").length, canvas: c ? Array.from(c.getContext("2d").getImageData(15, 20, 1, 1).data).join(",") : "none", origin: location.origin };
  await app.callServerTool({ name: "probe_report", arguments: { report: "pluto " + JSON.stringify(r) } });
}, 6000);
</script>'''.replace('/*APPLIB*/', lib.replace('</script', '<\\/script'))
p = p.replace('</body>', tail + '</body>', 1) if '</body>' in p else p + tail
open('pluto.built.html', 'w').write(p)
print(len(v), len(p))
