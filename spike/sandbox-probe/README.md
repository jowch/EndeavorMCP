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
