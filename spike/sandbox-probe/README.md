# Sandbox probe (spike, not for merging)

A throwaway stdio MCP server whose two tools open MCP App views:

- `probe_sandbox`: tests what the host's view sandbox allows (eval, blob and
  data images, fonts, workers, wasm, loopback fetch, storage) and saves the
  results through `probe_report` to `report.log` next to `server.py`
  (or in `$PROBE_LOG_DIR`).
- `probe_pluto`: a demo Pluto notebook (Pluto 1.0.3 offline export, scripts and
  styles inlined, state fed through Pluto's injected-environment hook, a
  `Function` shim) inside a view; it reports cell count and a canvas pixel.

Setup: `gunzip -k pluto.built.html.gz`, then add to claude_desktop_config.json:
`"endeavor-probe": {"command": "python3", "args": ["/full/path/server.py"]}`.

Rebuild: `make.jl` (Julia, Pluto) writes the export, `inline.py` inlines it,
`build.py` adds the MCP Apps client (needs `applib.js`, see build.py).

Round 2: `probe_loader` opens a 340 KB view that pulls the same page in parts
through the app-only tool `page_part` and builds it in place (gunzip
`pluto.inner.html.gz` first). `probe_cdn` opens a 360 KB page that loads
Pluto's code from cdn.jsdelivr.net (declared in `_meta.ui.csp`).
`probe_size` with `mb` pads the probe view to that size, to find the limit.

Round 3 (live): `probe_live` opens a view running Pluto's real editor. A
WebSocket stand-in in the view carries Pluto's messages through the app-only
tools `ws_open`, `ws_send`, `ws_recv` (long poll); `wsrelay.py` holds the
real socket to Pluto on 127.0.0.1. Start Pluto with
`SPIKE_DIR=$PWD julia --project=../../runtime live.jl`, wait for "ready",
run `./make-live.sh`. `probe_loader` now reads `static.parts.json`
(gunzip `static.parts.json.gz`), already split into head, body and scripts so the
view never parses the big bundles as HTML. Tested here in headless Chromium:
cells render, a slider change reruns the dependent cell in Julia.
