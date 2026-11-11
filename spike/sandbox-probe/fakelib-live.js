window.McpAppLib = { App: class { constructor(){}; async connect(){};
  async callServerTool({ name, arguments: a }) { const r = await window.parent.__mcpCall(name, JSON.stringify(a || {})); return JSON.parse(r); } } };
