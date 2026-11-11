window.McpAppLib = { App: class { constructor(){}; async connect(){}; async callServerTool({ name, arguments: a }) {
  if (name === "page_part") { const full = window.parent.__FULL; return { structuredContent: { total: full.length, text: full.slice(a.offset, a.offset + a.size) } }; }
  if (name === "probe_report") { window.parent.__REPORT = a.report; return {}; } } } };
