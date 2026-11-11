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
