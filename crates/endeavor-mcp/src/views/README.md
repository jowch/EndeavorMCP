# The notebook view

`notebook.html` is the view a client that draws MCP App views shows for
`open_notebook` and `new_notebook` (see `../views.rs`). The binary serves it
with `mcp-app.js` inside.

`mcp-app.js` is `dist/src/app-with-deps.js` from
[`@modelcontextprotocol/ext-apps`](https://github.com/modelcontextprotocol/ext-apps)
1.7.5 (MIT; the project is moving to Apache-2.0), unchanged. `views.rs` turns
its closing `export{...}` into `window.McpAppLib={...}` so it runs inline. To
update it, take the same file from a newer release and run
`node scripts/notebook-view-check.cjs`.

`endeavor-view.js` is the Endeavor app's `frontend/dist/view.js` (built from its
`frontend/src/view.ts`): the Endeavor look and Point on Pluto's page. The app's
repo is its source; change it there, then `sh scripts/view-frontend.sh sync`.
CI fails while this copy differs from the app's `main`. Whoever merges an app
PR that changes `dist/view.js` opens the EndeavorMCP PR that syncs it.

## How the view reaches Pluto

The view runs in the client's sandbox, which can reach nothing on this
computer, so it gets everything through tools only it can call
(`visibility: ["app"]`): an engine's page in parts (`notebook_page`), and a
WebSocket to Pluto that the core holds for it (`notebook_socket_*`). The
front (`endeavor mcp`) serves the view itself (`resources/read`) and forwards
these calls to the core. The sockets live in the core, since a client may run
one front per surface, and in Claude Desktop's chat the agent's calls and the
view's reach different fronts.

- A socket goes only to this runtime's own Pluto, on loopback, with Pluto's
  secret, as the page proxy does; the view names a notebook, never an
  address. Opening one checks that Pluto has that notebook open, but the
  socket is then a whole Pluto connection, as a browser tab's is: its
  messages can name any notebook this Pluto has open. The browser link behind
  the same secret gives that too, and no more.
- Pluto's page is its offline page (`generate_html(offline_bundle=true)`),
  sent as JSON the view puts together: `head` and `body` without scripts, and
  the scripts in order. The view can't parse the page itself: Pluto's code
  holds sequences (`<!--<script`) that change how HTML reads a script once it
  is inline. Here the scripts are still `data:` sources, so a script's end is
  the first `</script` after it. Ember's page comes from R's adapter, made
  once per start of R: each start may have a newer Ember, and may reuse the
  port (Ember tries 4321 first).
- A socket closes itself when its view hasn't asked about it for a minute, or
  has left too much unread: the view went away without saying so (Claude
  quit, say). A long-poll waits at most 1.5 s, and a large message comes in
  parts, as the page does, since a client may limit a tool result's size.
- Only Ember serves a notebook's widget files (`deps/<name>-<version>/<file>`,
  read from Ember's server with its secret); Pluto's come with the page.
- The pictures of boxes the user draws with Point are kept an hour for the
  agent's `view_cell_output` with `crop`, since the view's frame can reach
  only our tools. The view names each one with 16 random hex digits, so its
  message needn't wait for the call: Claude's desktop app takes a view's
  message only just after the user's press.

## What Claude's desktop app does with it (checked 2026-11-12)

- It shows the view's title in the call's row, after "Widget from <server>".
- A client may say it draws views and still show no card (the desktop app's
  Chat tab, #311), so `open_notebook` and `new_notebook` take an argument to
  open the notebook in the browser after all.
- The view asks to open in the side panel (Claude's own field, and
  `availableDisplayModes`). The app ignores `prefersBorder` for a local
  server and keeps its bordered frame.
- The frame loads nothing from the web unless its `csp` names the host, which
  the app honours for a local server. jsDelivr serves Pluto's fonts and
  MathJax's speech-rule maps, Ember's MathJax, some PlutoUI widgets' scripts
  (RangeSlider, Scrubbable) and the font of Masque's controls; esm.sh serves
  plotly.js, which PlutoPlotly imports unless a cell runs
  `enable_plutoplotly_offline()`. Only jsDelivr is fetched from (MathJax's
  maps). Other hosts are left out.
