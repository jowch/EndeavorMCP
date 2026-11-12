#!/usr/bin/env node
// Checks the notebook view (crates/endeavor-mcp/src/views.rs) end to end without Claude: it starts
// `endeavor mcp` as a client that draws MCP App views would, opens a notebook, loads the view in
// a sandboxed frame under a strict content security policy (nothing reachable but our own tools,
// as in Claude), and plays the host's side of the MCP Apps protocol. Then it moves a slider and
// checks that the dependent cell ran again in Julia, and points at an output with Point and checks
// what reaches the chat. With --r it does the same with an R notebook in Ember, and edits a cell in
// the page where the Julia notebook moves its slider.
//
//   node scripts/notebook-view-check.cjs [path/to/endeavor] [--r]
//   (JULIA_DEPOT=... sets the runtime's depot, as `--depot` does)
//
// Needs Playwright with Chromium, and a Julia for the runtime (the first run installs Pluto), or
// with --r an Rscript (the first run installs Ember).
// Writes its notebook, state and a screenshot to a temporary folder, and prints what it saw.
const { chromium } = require("playwright");
const { spawn } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

const args = process.argv.slice(2);
const R = args.includes("--r");
const binary = path.resolve(args.find(a => a !== "--r") || path.join(__dirname, "..", "target", "debug", "endeavor"));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "endeavor-view-"));
const notebookPath = path.join(dir, R ? "view-check.R" : "view-check.jl");
// The same three cells in each language: a title, `n`, and `squares`, which reads `n`.
const SQUARES = "6e6a1b70-dda7-11f1-a148-b56bbb7771a3";
const LANGUAGE = R ? "R" : "Julia";
fs.writeFileSync(notebookPath, R ? `### An Ember notebook ###
# /// environment
# snapshot = "2026-11-01"
# ///

# %% id=6e6a1b70-dda7-11f1-a148-b56bbb7771a1
"View check"

# %% id=6e6a1b70-dda7-11f1-a148-b56bbb7771a2
n <- 5

# %% id=${SQUARES}
squares <- (1:n)^2
squares

# /// cell order
# 6e6a1b70-dda7-11f1-a148-b56bbb7771a1
# 6e6a1b70-dda7-11f1-a148-b56bbb7771a2
# ${SQUARES}
# ///
` : `### A Pluto.jl notebook ###
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
  for (let attempt = 0; attempt < (R ? 240 : 30); attempt++) {
    opened = await rpc("tools/call", { name: "open_notebook", arguments: { path: notebookPath, run_notebook: true } });
    const text = opened.result?.content?.[0]?.text || "";
    if (!/notebook_opening|julia_starting|starting|r_installing/.test(text)) break;
    await new Promise(r => setTimeout(r, R ? 5000 : 2000));
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
  let failOnce = null;
  await page.exposeFunction("toolsCall", async params => {
    calls[params.name] = (calls[params.name] || 0) + 1;
    // A connection that fails once, as Pluto's would if it were restarting.
    if (failOnce === params.name) { failOnce = null; return { content: [{ type: "text", text: "not_connected: couldn't reach Pluto: Connection refused (os error 111)" }], isError: true }; }
    const t = Date.now();
    const result = (await rpc("tools/call", params)).result;
    if (calls[params.name] === 1) firsts[params.name] = [t, Date.now() - t];
    return result;
  });
  // What Claude's sandbox allows, as measured on Claude Desktop: inline scripts and eval, data: and
  // blob:, and nothing from any host (no connect-src beyond the frame itself).
  const csp = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' blob: data:; style-src 'unsafe-inline'; img-src data: blob:; font-src 'none'; connect-src 'none'; worker-src blob:; media-src data: blob:";
  const html = view.text.replace("<head>", `<head><meta http-equiv="Content-Security-Policy" content="${csp}">`);
  await page.setContent(`<!DOCTYPE html><body style="margin:0"><textarea id=chat-box></textarea><iframe id=view sandbox="allow-scripts allow-same-origin" style="width:1000px;height:850px;border:0"></iframe>
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
  if (m.method === "ui/notifications/size-changed") { window.sizes = (window.sizes || []).concat([m.params]); return; }
  if (m.method === "tools/call") return send({ id: m.id, result: await window.toolsCall(m.params) });
  if (m.method === "ui/message" && window.refuse) { window.refuse--; return send({ id: m.id, result: { isError: true } }); }
  if (m.method === "ui/message" && window.fail) { window.fail = false; window.failed = (window.failed || 0) + 1; return send({ id: m.id, error: { code: -32001, message: "Request timed out" } }); }
  if (m.method === "ui/message" && window.slow) await new Promise(r => setTimeout(r, window.slow));
  if (m.method === "ui/message") { window.messages.push(m.params); return send({ id: m.id, result: {} }); }
  if (m.method === "ui/open-link") { (window.links ||= []).push(m.params.url); return send({ id: m.id, result: {} }); }
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
  const frame = page.frames()[1];
  // A first start that goes on by itself (Ember installing, here): the card says so, not that the call failed.
  await page.evaluate(() => window.sendToolResult({ content: [{ type: "text", text: JSON.stringify({ error: "r_installing", message: "Installing Ember for R notebooks, which takes a few minutes the first time. Make the same call again right away." }) }], isError: false }));
  await frame.waitForFunction(() => /Setting up R notebooks/.test(document.querySelector("#endeavor-card .note")?.textContent || ""), null, { timeout: 10000 })
    .catch(() => {});
  const installing = await frame.evaluate(() => [document.querySelector("#endeavor-card .note")?.textContent, document.querySelector("#endeavor-open").hidden]);
  check(/^Setting up R notebooks\. .*Claude calls again, and the notebook opens in a new card\.$/.test(installing[0] || "") && installing[1], `an install in progress says so, with no button: ${JSON.stringify(installing)}`);
  await page.evaluate(result => window.sendToolResult(result), opened.result);
  await frame.waitForFunction(() => document.querySelector("#endeavor-open") && !document.querySelector("#endeavor-open").hidden, null, { timeout: 10000 });
  const asked = await page.evaluate(() => window.modes.slice());
  check(asked[0] === "fullscreen", "the view asks for the panel as it loads");
  check(!(calls.notebook_page || calls.notebook_socket_open), "inline, it is a card: no page loaded, no socket");
  // The user's click on the card: the host moves it.
  await page.evaluate(() => { window.allowMove = true; });
  failOnce = "notebook_socket_open";
  await frame.click("#endeavor-open");
  await frame.waitForFunction(() => document.querySelector("#endeavor-card.cover") && !document.querySelector("#endeavor-open").hidden, null, { timeout: 10000 });
  const card = await frame.evaluate(() => [document.querySelector("#endeavor-card .note").textContent, document.querySelector("#endeavor-open").textContent]);
  check(card[0] === "Couldn't connect to the notebook. Try again, or ask Claude to open it again." && card[1] === "Try again", `a failed load says so in plain words, with a way to try again: ${JSON.stringify(card)}`);
  const clicked = Date.now();
  await frame.click("#endeavor-open");
  await frame.waitForFunction(() => document.querySelectorAll("pluto-cell").length === 3, null, { timeout: 60000 });
  const cellsAt = Date.now() - clicked;
  // Once the page is connected, Pluto sets the slider's value in Julia and the cell runs again. Ember
  // shows the result as R prints it.
  const showsSquares = R ? /1\s+4\s+9\s+16\s+25/ : /squares[\s\S]*25/;
  await frame.waitForFunction(re => document.querySelectorAll("pluto-cell").length === 3 && new RegExp(re).test([...document.querySelectorAll("pluto-output")].map(o => o.innerText).join(" ")), showsSquares.source, { timeout: 60000 });
  const shown = Date.now() - clicked;
  const before = await frame.evaluate(() => [...document.querySelectorAll("pluto-output")].map(o => o.innerText.replace(/\s+/g, " ").trim()));
  check(before.some(t => showsSquares.test(t)), `the live notebook shows in the panel (${shown} ms after the click): ${JSON.stringify(before[2])}`);
  const moved = Date.now();
  if (R) {
    // An edit in the page, run with Shift+Enter: it goes to Ember through the relay, and `squares` runs again.
    const editor = frame.locator("pluto-cell").nth(1).locator(".cm-content");
    await editor.click();
    await page.keyboard.press("Control+A");
    await page.keyboard.type("n <- 12");
    await page.keyboard.press("Shift+Enter");
  } else {
    await frame.evaluate(() => { const r = document.querySelector("input[type=range]"); r.value = 12; r.dispatchEvent(new Event("input", { bubbles: true })); });
  }
  await frame.waitForFunction(() => /144/.test([...document.querySelectorAll("pluto-output")].map(o => o.innerText).join(" ")), null, { timeout: 20000 });
  check(true, `${R ? "editing n in the page" : "moving the slider"} ran the dependent cell again in ${LANGUAGE} (${Date.now() - moved} ms)`);
  await page.screenshot({ path: path.join(dir, "view.png") });

  // The Endeavor look, filling the panel: Pluto's header and footer give way to the view's toolbar,
  // with the file's name, Docs and Point.
  const look = await frame.evaluate(() => ({
    colours: (document.getElementById("endeavor-theme")?.textContent || "").includes("--main-bg-color"),
    // Ember's page has no footer: one that isn't there is hidden too.
    header: [document.querySelector("header#pluto-nav")].some(e => e && getComputedStyle(e).display !== "none"),
    footer: [document.querySelector("pluto-editor footer")].some(e => e && getComputedStyle(e).display !== "none"),
    toolbar: document.querySelector("#endeavor-bar .file")?.textContent,
    button: !!document.querySelector("#endeavor-bar #endeavor-point"),
  }));
  check(look.colours && !look.header && !look.footer && look.toolbar === path.basename(notebookPath) && look.button, `the Endeavor look is on, and the toolbar (name, Docs, Point) stands in for Pluto's header and footer (${JSON.stringify(look)})`);
  // Point: the button, a click on the squares cell's output, a comment, ↩.
  await frame.click("#endeavor-point");
  check(await frame.evaluate(() => document.body.classList.contains("annotating")), "the button turns Point on");
  const output = frame.locator("pluto-cell").nth(2).locator("pluto-output");
  await output.click();
  const what = await frame.evaluate(() => document.querySelector("#annotate-bar .status")?.textContent);
  check(/squares/.test(what || ""), `a click picks the output (${what})`);
  await frame.locator("#annotate-bar textarea").fill("Why squares?");
  // The chat takes a moment to answer (Claude's desktop app takes seconds to show the text): the view says it's sending.
  await page.evaluate(() => { window.slow = 800; });
  await frame.locator("#annotate-bar textarea").press("Enter");
  const sending = await frame.evaluate(() => document.getElementById("endeavor-sent")?.textContent);
  check(/^Sending/.test(sending || ""), `while the chat hasn't answered, the view says it's sending (${sending})`);
  await page.waitForFunction(() => window.messages.length === 1, null, { timeout: 5000 }).catch(() => {});
  await frame.waitForFunction(() => /message box/.test(document.getElementById("endeavor-sent")?.textContent || ""), null, { timeout: 5000 }).catch(() => {});
  check(await frame.evaluate(() => document.body.classList.contains("annotating")), "once the chat has it, Point stays on while the keyboard is still in the view");
  // The user goes to Claude's message box to send it: Point ends, as their Escape there can't reach the view.
  await page.focus("#chat-box");
  await frame.waitForFunction(() => !document.body.classList.contains("annotating"), null, { timeout: 5000 }).catch(() => {});
  check(await frame.evaluate(() => !document.body.classList.contains("annotating")), "once the keyboard leaves for Claude's box, Point ends");
  await page.evaluate(() => { window.slow = 0; });
  await frame.click("#endeavor-point");
  const message = await page.evaluate(() => window.messages[0]);
  const said = message?.content?.[0]?.text || "";
  check(message?.role === "user" && said.startsWith("\u2193\n\n[Endeavor] The user quoted") && said.includes("A \u2193 just before this note only marks where the quotes start") && new RegExp(`<quote cell="squares[^"]*" part="output" uri="notebook://${R ? "ember" : "pluto"}/[^"]+/cell/${SQUARES}">\\n[\\s\\S]+\\n</quote>\\nWhy squares\\?$`).test(said), `↩ sends the pick and the comment as the user's message, on a new paragraph: ${JSON.stringify(said.slice(-220))}`);
  // ⌘↩ (Ctrl+Enter off macOS) keeps the pick in the view; the next ↩ passes both in one message.
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await frame.locator("#annotate-bar textarea").fill("First");
  await frame.locator("#annotate-bar textarea").press(process.platform === "darwin" ? "Meta+Enter" : "Control+Enter");
  await new Promise(r => setTimeout(r, 500));
  check((await page.evaluate(() => window.messages.length)) === 1, "⌘↩ passes nothing yet");
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await frame.locator("#annotate-bar textarea").fill("Second");
  await frame.locator("#annotate-bar textarea").press("Enter");
  await page.waitForFunction(() => window.messages.length === 2, null, { timeout: 5000 }).catch(() => {});
  const second = await page.evaluate(() => window.messages[1]?.content?.[0]?.text || "");
  check(/\nFirst\n\n<quote[\s\S]*\nSecond$/.test(second) && second.split("[Endeavor]").length === 2 && (await page.evaluate(() => window.contexts.length)) === 0, "the next ↩ passes both picks in one message, under one note");
  // A send the chat refuses (as Claude's desktop app does for a few seconds after typing in its box) isn't
  // tried again: Point gets the pick and comment back, and the next ↩ sends them once.
  await page.evaluate(() => { window.refuse = 1; });
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await frame.locator("#annotate-bar textarea").fill("Kept");
  await frame.locator("#annotate-bar textarea").press("Enter");
  await frame.waitForFunction(() => document.querySelector("#annotate-bar textarea").value === "Kept", null, { timeout: 5000 }).catch(() => {});
  await new Promise(r => setTimeout(r, 1000));
  const back = await frame.evaluate(() => ({ comment: document.querySelector("#annotate-bar textarea").value, what: document.querySelector("#annotate-bar .status")?.textContent, said: document.getElementById("endeavor-sent")?.textContent }));
  check(back.comment === "Kept" && /squares/.test(back.what || "") && /try again in a few seconds\. Your pick is back/.test(back.said || "") && (await page.evaluate(() => window.messages.length === 2 && window.refuse === 0)), `a refused send isn't tried again, and Point gets its pick and comment back (${JSON.stringify(back)})`);
  await frame.locator("#annotate-bar textarea").press("Enter");
  await page.waitForFunction(() => window.messages.length === 3, null, { timeout: 5000 }).catch(() => {});
  const again = await page.evaluate(() => window.messages[2]?.content?.[0]?.text || "");
  check(again.split("<quote cell").length === 2 && again.endsWith("\nKept"), "↩ then sends them, once");
  // A box (or a figure) goes at once: the view takes no picture, which would hold the message back past
  // the moment Claude's desktop app takes it.
  const outBox = await frame.locator("pluto-cell").nth(2).locator("pluto-output").boundingBox();
  await page.mouse.move(outBox.x + 2, outBox.y + 2);
  await page.mouse.down();
  await page.mouse.move(outBox.x + 60, outBox.y + 30, { steps: 5 });
  await page.mouse.up();
  const boxAt = Date.now();
  await frame.locator("#annotate-bar textarea").press("Enter");
  await page.waitForFunction(() => window.messages.length === 4, null, { timeout: 5000 }).catch(() => {});
  const boxed = await page.evaluate(() => window.messages[3]?.content?.[0]?.text || "");
  check(/<quote part="box" uri=/.test(boxed) && Date.now() - boxAt < 1500, `a box goes at once, with no picture (${Date.now() - boxAt} ms)`);
  // Nor is one that fails any other way (a timeout), which could still arrive late.
  await page.evaluate(() => { window.fail = true; });
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await frame.locator("#annotate-bar textarea").fill("Timed out");
  await frame.locator("#annotate-bar textarea").press("Enter");
  await frame.waitForFunction(() => document.querySelector("#annotate-bar textarea").value === "Timed out", null, { timeout: 5000 }).catch(() => {});
  await new Promise(r => setTimeout(r, 1000));
  check((await page.evaluate(() => window.failed === 1 && window.messages.length === 4)), "a timed-out send isn't tried again, and its pick comes back");
  await frame.locator("pluto-cell").nth(2).locator("pluto-output").click();
  await page.screenshot({ path: path.join(dir, "point.png") });
  await frame.locator("#annotate-bar textarea").press("Escape");
  check(await frame.evaluate(() => !document.body.classList.contains("annotating")), "Escape leaves Point");
  // A link in a cell's output opens through Claude (the frame can't open windows). No other link may load a
  // page in the frame, which would replace the view with a blank one: a jump within the page scrolls.
  await frame.evaluate(() => document.querySelector("pluto-cell pluto-output").insertAdjacentHTML("beforeend",
    '<a id="out-link" href="https://example.com/docs">docs</a> <a id="in-link" href="#out-link">here</a> <a id="empty-link" href="#">top</a> <a id="rel-link" href="./other.jl">other</a> <a id="file-link" href="file:///etc/hosts">file</a>'));
  for (const id of ["out-link", "in-link", "empty-link", "rel-link", "file-link"]) await frame.click("#" + id);
  await page.waitForFunction(() => (window.links || []).length > 0, null, { timeout: 5000 }).catch(() => {});
  await new Promise(r => setTimeout(r, 500));
  const links = await page.evaluate(() => window.links || []);
  check(links.length === 1 && links[0] === "https://example.com/docs" && await frame.evaluate(() => !!document.querySelector("pluto-editor")), `a link in a cell opens through Claude's ui/open-link, and no other link replaces the view (${JSON.stringify(links)})`);
  await page.evaluate(() => window.closePane());
  await frame.waitForFunction(() => !document.querySelector("#endeavor-card").hidden && !document.querySelector("#endeavor-open").hidden, null, { timeout: 10000 });
  // The size the view reports, which the host gives its frame: the card's, not the panel's it had.
  await new Promise(r => setTimeout(r, 800));
  const reported = await page.evaluate(() => (window.sizes || []).at(-1)?.height);
  const small = await frame.evaluate(() => document.documentElement.classList.contains("endeavor-inline") && [...document.body.children].filter(e => e.id !== "endeavor-card").every(e => getComputedStyle(e).display === "none"));
  check(small && reported < 200, `with the pane closed, the card and its button are back, at card size (reports ${reported} px)`);
  // Back in the pane, Claude closes the notebook: the view says so, instead of Pluto's endless "Reconnecting…".
  await frame.click("#endeavor-open");
  await frame.waitForFunction(() => document.querySelector("#endeavor-card").hidden, null, { timeout: 10000 });
  const closed = (await rpc("tools/call", { name: "close_notebook", arguments: { notebook_id: fields.notebook_id } })).result;
  check(!closed.isError, "close_notebook closed it");
  await frame.waitForFunction(() => /was closed/.test(document.querySelector("#endeavor-card.cover:not([hidden]) .note")?.textContent || ""), null, { timeout: 30000 }).catch(() => {});
  const shut = await frame.evaluate(() => ({ note: document.querySelector("#endeavor-card .note").textContent, cover: document.querySelector("#endeavor-card").className, button: !document.querySelector("#endeavor-open").hidden }));
  check(shut.cover === "cover" && /was closed/.test(shut.note) && !shut.button, `once it's closed, the view says so (${JSON.stringify(shut)})`);
  await page.screenshot({ path: path.join(dir, "closed.png") });
  console.log("screenshots " + ["view.png", "point.png", "closed.png"].map(f => path.join(dir, f)).join(" "));
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
