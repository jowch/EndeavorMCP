#!/usr/bin/env node
// Checks the notebook view (crates/endeavor-mcp/src/views.rs) end to end without Claude: it starts
// `endeavor mcp` as a client that draws MCP App views would, opens a notebook, loads the view in
// a sandboxed frame under a strict content security policy (nothing reachable but our own tools,
// as in Claude), and plays the host's side of the MCP Apps protocol. Then it moves a slider and
// checks that the dependent cell ran again in Julia, and points at an output with Point and checks
// what reaches the chat.
//
//   node scripts/notebook-view-check.cjs [path/to/endeavor]
//   (JULIA_DEPOT=... sets the runtime's depot, as `--depot` does)
//
// Needs Playwright with Chromium and a Julia for the runtime (the first run installs Pluto).
// Writes its notebook, state and a screenshot to a temporary folder, and prints what it saw.
const { chromium } = require("playwright");
const { spawn } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

const binary = path.resolve(process.argv[2] || path.join(__dirname, "..", "target", "debug", "endeavor"));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "endeavor-view-"));
const notebookPath = path.join(dir, "view-check.jl");
fs.writeFileSync(notebookPath, `### A Pluto.jl notebook ###
# v0.20.0

using Markdown
using InteractiveUtils

# This Pluto notebook uses @bind for interactivity. When running this notebook outside of Pluto, the following 'mock version' of @bind gives bound variables a default value (instead of an error).
macro bind(def, element)
    quote
        local iv = try Base.loaded_modules[Base.PkgId(Base.UUID("6e696c72-6542-2067-7265-42206c756150"), "AbstractPlutoDingetjes")].Bonds.initial_value catch; b -> missing; end
        local el = $(esc(element))
        global $(esc(def)) = Core.applicable(Base.get, el) ? Base.get(el) : iv(el)
        el
    end
end

# ╔═╡ 6e6a1b70-dda7-11f1-a148-b56bbb7771a1
md"# View check"

# ╔═╡ 6e6a1b70-dda7-11f1-a148-b56bbb7771a2
@bind n html"<input type=range min=1 max=20 value=5>"

# ╔═╡ 6e6a1b70-dda7-11f1-a148-b56bbb7771a3
squares = [i^2 for i in 1:something(n, 5)]

# ╔═╡ Cell order:
# ╠═6e6a1b70-dda7-11f1-a148-b56bbb7771a1
# ╠═6e6a1b70-dda7-11f1-a148-b56bbb7771a2
# ╠═6e6a1b70-dda7-11f1-a148-b56bbb7771a3
`);

const depot = process.env.JULIA_DEPOT ? ["--depot", process.env.JULIA_DEPOT] : [];
const front = spawn(binary, ["mcp", "--folder", dir, "--state-dir", path.join(dir, "state"), ...depot], { stdio: ["pipe", "pipe", "pipe"] });
let stderr = "";
front.stderr.on("data", d => { stderr += d; });
let nextId = 1, buffer = "";
const pending = new Map();
front.stdout.on("data", d => {
  buffer += d;
  let i;
  while ((i = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, i); buffer = buffer.slice(i + 1);
    if (!line.trim()) continue;
    const message = JSON.parse(line);
    pending.get(message.id)?.(message); pending.delete(message.id);
  }
});
const rpc = (method, params) => new Promise(resolve => { const id = nextId++; pending.set(id, resolve); front.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n"); });
const fail = why => { console.error("FAIL: " + why); console.error(stderr.slice(-3000)); front.kill(); process.exit(1); };
const check = (ok, what) => { console.log((ok ? "ok   " : "FAIL ") + what); if (!ok) fail(what); };

(async () => {
  const init = await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: { extensions: { "io.modelcontextprotocol/ui": { mimeTypes: ["text/html;profile=mcp-app"] } } }, clientInfo: { name: "view-check", version: "0" } });
  check(init.result?.capabilities?.resources, "initialize offers resources to a client that draws views");
  front.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
  const tools = (await rpc("tools/list", {})).result.tools;
  const open = tools.find(t => t.name === "open_notebook");
  check(open?._meta?.ui?.resourceUri === "ui://endeavor/notebook", "open_notebook shows the notebook view");
  check(tools.filter(t => ["notebook_page", "notebook_socket_open", "notebook_socket_send", "notebook_socket_recv", "notebook_socket_close"].includes(t.name)).length === 5 && tools.filter(t => ["notebook_page", "notebook_socket_open", "notebook_socket_send", "notebook_socket_recv", "notebook_socket_close"].includes(t.name)).every(t => t._meta?.ui?.visibility?.[0] === "app"), "the view's tools are for the view only");
  const view = (await rpc("resources/read", { uri: "ui://endeavor/notebook" })).result.contents[0];
  check(view.mimeType === "text/html;profile=mcp-app" && view.text.length > 100000, `the view is served (${view.text.length} bytes)`);

  const began = Date.now();
  let opened;
  for (let attempt = 0; attempt < 30; attempt++) {
    opened = await rpc("tools/call", { name: "open_notebook", arguments: { path: notebookPath, run_notebook: true } });
    const text = opened.result?.content?.[0]?.text || "";
    if (!/notebook_opening|julia_starting|starting/.test(text)) break;
    await new Promise(r => setTimeout(r, 2000));
  }
  console.log("open_notebook said: " + JSON.stringify(opened.result).slice(0, 600));
  const fields = JSON.parse(opened.result.content[0].text);
  check(!opened.result.isError && fields.notebook_id, `open_notebook opened it (${Math.round((Date.now() - began) / 1000)} s)`);
  check(fields.opened_in_browser === false && fields.shown_beside_chat === true, "no browser tab: it shows beside the chat");

  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1100, height: 900 } });
  const errors = [];
  page.on("pageerror", e => errors.push(e.message.slice(0, 200)));
  const calls = {};
  const firsts = {};
  await page.exposeFunction("toolsCall", async params => {
    calls[params.name] = (calls[params.name] || 0) + 1;
    const t = Date.now();
    const result = (await rpc("tools/call", params)).result;
    if (calls[params.name] === 1) firsts[params.name] = [t, Date.now() - t];
    return result;
  });
  // What Claude's sandbox allows, as measured on Claude Desktop: inline scripts and eval, data: and
  // blob:, and nothing from any host (no connect-src beyond the frame itself).
  const csp = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' blob: data:; style-src 'unsafe-inline'; img-src data: blob:; font-src 'none'; connect-src 'none'; worker-src blob:; media-src data: blob:";
  const html = view.text.replace("<head>", `<head><meta http-equiv="Content-Security-Policy" content="${csp}">`);
  await page.setContent(`<!DOCTYPE html><body style="margin:0"><iframe id=view sandbox="allow-scripts allow-same-origin" style="width:1000px;height:850px;border:0"></iframe>
<script>
const frame = document.getElementById("view");
const context = { displayMode: "inline", availableDisplayModes: ["inline", "fullscreen"], containerDimensions: { width: 1000, maxHeight: 850 }, platform: "desktop" };
window.modes = [];
window.messages = [];
window.contexts = [];
const send = m => frame.contentWindow.postMessage(Object.assign({ jsonrpc: "2.0" }, m), "*");
window.sendToolResult = result => send({ method: "ui/notifications/tool-result", params: result });
// The user closes the side pane: the view is inline again.
window.closePane = () => { context.displayMode = "inline"; send({ method: "ui/notifications/host-context-changed", params: { displayMode: "inline" } }); };
window.addEventListener("message", async e => {
  if (e.source !== frame.contentWindow) return;
  const m = e.data;
  if (!m || m.jsonrpc !== "2.0" || m.method === undefined) return;
  if (m.method === "ui/initialize") return send({ id: m.id, result: { protocolVersion: m.params.protocolVersion, hostInfo: { name: "view-check", version: "0" }, hostCapabilities: { serverTools: {}, openLinks: {}, logging: {}, message: { text: {} }, updateModelContext: { text: {} } }, hostContext: context } });
  if (m.method === "ui/notifications/initialized") { window.initialized = true; return; }
  if (m.method === "tools/call") return send({ id: m.id, result: await window.toolsCall(m.params) });
  if (m.method === "ui/message" && window.refuse) { window.refuse = false; return send({ id: m.id, error: { code: -32000, message: "refused" } }); }
  if (m.method === "ui/message") { window.messages.push(m.params); return send({ id: m.id, result: {} }); }
  if (m.method === "ui/update-model-context") { window.contexts.push(m.params); return send({ id: m.id, result: {} }); }
  if (m.method === "ui/request-display-mode") {
    window.modes.push(m.params.mode);
    // As the Code tab does: a request with no click in the view is refused.
    const granted = window.allowMove ? m.params.mode : context.displayMode;
    send({ id: m.id, result: { mode: granted } });
    if (granted !== context.displayMode) { context.displayMode = granted; send({ method: "ui/notifications/host-context-changed", params: { displayMode: granted } }); }
    return;
  }
  if (m.id !== undefined) send({ id: m.id, result: {} });
});
</script></body>`);
  await page.$eval("#view", (f, s) => { f.srcdoc = s; }, html);
  await page.waitForFunction(() => window.initialized, null, { timeout: 15000 });
  await page.evaluate(result => window.sendToolResult(result), opened.result);
  const frame = page.frames()[1];
  await frame.waitForFunction(() => document.querySelector("#endeavor-open") && !document.querySelector("#endeavor-open").hidden, null, { timeout: 10000 });
  const asked = await page.evaluate(() => window.modes.slice());
  check(asked[0] === "fullscreen", "the view asks for the panel as it loads");
  check(!(calls.notebook_page || calls.notebook_socket_open), "inline, it is a card: no page loaded, no socket");
  // The user's click on the card: the host moves it.
  await page.evaluate(() => { window.allowMove = true; });
  const clicked = Date.now();
  await frame.click("#endeavor-open");
  await frame.waitForFunction(() => document.querySelectorAll("pluto-cell").length === 3, null, { timeout: 60000 });
  const cellsAt = Date.now() - clicked;
  // Once the page is connected, Pluto sets the slider's value in Julia and the cell runs again.
  await frame.waitForFunction(() => document.querySelectorAll("pluto-cell").length === 3 && /squares[\s\S]*25/.test([...document.querySelectorAll("pluto-output")].map(o => o.innerText).join(" ")), null, { timeout: 60000 });
  const shown = Date.now() - clicked;
  const before = await frame.evaluate(() => [...document.querySelectorAll("pluto-output")].map(o => o.innerText.replace(/\s+/g, " ").trim()));
  check(before.some(t => /squares.*25/.test(t)), `the live notebook shows in the panel (${shown} ms after the click): ${JSON.stringify(before[2])}`);
  const moved = Date.now();
  await frame.evaluate(() => { const r = document.querySelector("input[type=range]"); r.value = 12; r.dispatchEvent(new Event("input", { bubbles: true })); });
  await frame.waitForFunction(() => /144/.test([...document.querySelectorAll("pluto-output")].map(o => o.innerText).join(" ")), null, { timeout: 20000 });
  check(true, `moving the slider ran the dependent cell again in Julia (${Date.now() - moved} ms)`);
  await page.screenshot({ path: path.join(dir, "view.png") });

  // The Endeavor look, with Pluto's own header (the view has no app header to stand in for it).
  const look = await frame.evaluate(() => ({
    colours: (document.getElementById("endeavor-theme")?.textContent || "").includes("--main-bg-color"),
    header: getComputedStyle(document.querySelector("header#pluto-nav") || document.body).display !== "none",
    button: !!document.getElementById("endeavor-point"),
  }));
  check(look.colours && look.header && look.button, `the Endeavor look is on, Pluto's header shows, and so does Point's button (${JSON.stringify(look)})`);
  // Point: the button, a click on the squares cell's output, a comment, ↩.
  await frame.click("#endeavor-point");
  check(await frame.evaluate(() => document.body.classList.contains("annotating")), "the button turns Point on");
  const output = frame.locator("pluto-cell").nth(2).locator("pluto-output");
  await output.click();
  const what = await frame.evaluate(() => document.querySelector("#annotate-bar .status")?.textContent);
  check(/squares/.test(what || ""), `a click picks the output (${what})`);
  await frame.locator("#annotate-bar textarea").fill("Why squares?");
  await frame.locator("#annotate-bar textarea").press("Enter");
  await page.waitForFunction(() => window.messages.length === 1, null, { timeout: 5000 }).catch(() => {});
  const message = await page.evaluate(() => window.messages[0]);
  const said = message?.content?.[0]?.text || "";
  check(message?.role === "user" && said.startsWith("[Endeavor] The user quoted") && /<quote cell="squares" part="output" uri="notebook:\/\/pluto\/[^"]+\/cell\/6e6a1b70-dda7-11f1-a148-b56bbb7771a3">\n[\s\S]+\n<\/quote>\nWhy squares\?$/.test(said), `↩ sends the pick and the comment as the user's message: ${JSON.stringify(said.slice(-220))}`);
  // ⌘↩ (Ctrl+Enter off macOS): to the chat as well, where the user adds to the message.
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await frame.locator("#annotate-bar textarea").fill("Later");
  await frame.locator("#annotate-bar textarea").press(process.platform === "darwin" ? "Meta+Enter" : "Control+Enter");
  await page.waitForFunction(() => window.messages.length === 2, null, { timeout: 5000 }).catch(() => {});
  const second = await page.evaluate(() => window.messages[1]?.content?.[0]?.text || "");
  check(/Later$/.test(second) && (await page.evaluate(() => window.contexts.length)) === 0, "⌘↩ passes it to the chat too, and gives no context");
  // A send the chat refuses gives the comment back.
  await page.evaluate(() => { window.refuse = true; });
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await frame.locator("#annotate-bar textarea").fill("Try again");
  await frame.locator("#annotate-bar textarea").press("Enter");
  await frame.waitForFunction(() => document.querySelector("#annotate-bar textarea").value === "Try again", null, { timeout: 5000 }).catch(() => {});
  check(await frame.evaluate(() => document.querySelector("#annotate-bar textarea").value === "Try again"), "a refused send puts the comment back in the box");
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await page.screenshot({ path: path.join(dir, "point.png") });
  await frame.locator("#annotate-bar textarea").press("Escape");
  check(await frame.evaluate(() => !document.body.classList.contains("annotating")), "Escape leaves Point");
  await page.evaluate(() => window.closePane());
  await frame.waitForFunction(() => !document.querySelector("#endeavor-card").hidden && !document.querySelector("#endeavor-open").hidden, null, { timeout: 10000 });
  const small = await frame.evaluate(() => document.documentElement.classList.contains("endeavor-inline") && [...document.body.children].filter(e => e.id !== "endeavor-card").every(e => getComputedStyle(e).display === "none"));
  check(small, "with the pane closed, the card and its button are back, at card size");
  console.log("screenshots " + path.join(dir, "view.png") + " " + path.join(dir, "point.png"));
  console.log("view calls " + JSON.stringify(calls));
  const at = name => firsts[name] ? `${firsts[name][0] - clicked} ms (took ${firsts[name][1]} ms)` : "never";
  console.log(`after the click: first page part at ${at("notebook_page")}, socket opened at ${at("notebook_socket_open")}, cells drawn at ${cellsAt} ms`);
  const relevant = errors.filter(e => !/plutojl|stats/.test(e));
  if (relevant.length) console.log("page errors: " + relevant.join(" | "));
  await browser.close();
  front.kill();
  spawn(binary, ["stop", "--force", "--state-dir", path.join(dir, "state")], { stdio: "ignore" });
  process.exit(0);
})().catch(e => fail(e.stack || String(e)));
