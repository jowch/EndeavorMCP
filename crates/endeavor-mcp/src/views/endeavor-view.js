"use strict";
(() => {
  // src/bridge.ts
  var handlers = {};
  var nonce = typeof __ENDEAVOR_NONCE__ === "string" ? __ENDEAVOR_NONCE__ : "";
  var handler = window.webkit?.messageHandlers?.ipc;
  var post = handler ? handler.postMessage.bind(handler) : (body) => window.ipc?.postMessage(body);
  var host = null;
  function setHost(handler2) {
    host = handler2;
  }
  function send(msg) {
    if (host) return host(msg);
    post(JSON.stringify(nonce ? { ...msg, nonce } : msg));
  }
  var byUser = (e) => e.isTrusted || !nonce && !host;
  function on(type, handler2) {
    (handlers[type] ??= []).push(handler2);
  }
  window.__endeavor = {
    receive(msg) {
      handlers[msg.type]?.forEach((handler2) => handler2(msg));
    }
  };

  // src/place.ts
  function barPlace(pick, width, height, barHeight) {
    const GAP = 8;
    const EDGE = 16;
    const barWidth = Math.max(Math.min(380, width - 32), 0);
    const left = Math.min(Math.max(pick.left, EDGE), width - EDGE - barWidth);
    const fitsBelow = pick.bottom + GAP + barHeight <= height - GAP;
    const fitsAbove = pick.top - GAP - barHeight >= GAP;
    const lowest = height - EDGE - barHeight;
    let top;
    if (pick.bottom < 0 || fitsBelow) top = pick.bottom + GAP;
    else if (pick.top > height || fitsAbove) top = pick.top - GAP - barHeight;
    else top = lowest;
    return { left: Math.max(left, EDGE), top: Math.min(Math.max(top, GAP), lowest), width: barWidth };
  }

  // src/keys.ts
  var mac = /Mac/.test(navigator.platform);
  function modHeld(e) {
    return mac ? e.metaKey : e.ctrlKey;
  }

  // src/reveal.ts
  function cellCode(cell) {
    const content = cell?.querySelector("pluto-input .cm-content");
    const view = content?.cmTile?.root?.view;
    if (view) return view.state.doc.toString();
    const lines = content ? [...content.querySelectorAll(".cm-line")].map((l) => l.textContent ?? "") : [];
    return lines.join("\n");
  }

  // src/cellname.ts
  function cellName(code) {
    const markdown = markdownText(code);
    let label;
    if (markdown !== null) {
      const lines = markdown.split("\n").map((l) => l.trim()).filter((l) => l);
      label = (lines.find((l) => l.startsWith("#")) ?? lines[0] ?? "markdown").replace(/^#+/, "").trim();
    } else {
      const first = codeLines(code)[0];
      label = commentName(code) ?? (first === void 0 ? "cell" : definedName(code) ?? inferredName(code) ?? first);
    }
    const chars = [...label];
    return chars.length > 28 ? `${chars.slice(0, 27).join("")}\u2026` : label;
  }
  function commentName(code) {
    const line = code.split("\n").map((l) => l.trim()).find((l) => l);
    if (!line?.startsWith("# ")) return null;
    const text = line.slice(2).trim();
    const codeLike = /^[\p{L}\p{N}_.!]+\(/u.test(text) || text.includes("=") || text.includes("<-") || ["using ", "import ", "library("].some((k) => text.startsWith(k));
    return text && !codeLike ? text : null;
  }
  function definedName(code) {
    const lines = codeLines(code);
    const line = lines[0] === "begin" ? lines[1] : lines[0];
    if (line === void 0) return null;
    const word = (s) => s.trim().match(/^[\p{L}\p{N}_!]+/u)?.[0] ?? null;
    const block = ["function ", "macro ", "struct ", "mutable struct "].find((k) => line.startsWith(k));
    if (block) return word(line.slice(block.length));
    const eq = line.indexOf("=");
    if (eq < 0) return null;
    const lhs = line.slice(0, eq);
    if (line[eq + 1] === "=" || lhs.split("(").length !== lhs.split(")").length) return null;
    return word(lhs.trim().replace(/^const /, ""));
  }
  var PLOTS = ["plot", "scatter", "heatmap", "histogram", "lines", "surface", "contour", "Figure"];
  function inferredName(code) {
    const lines = codeLines(code);
    if (lines.length < 2) return null;
    const plot = new RegExp(`(^|[^\\p{L}\\p{N}_.])(${PLOTS.join("|")})!*\\(`, "u");
    if (lines.some((l) => plot.test(l))) return "plot";
    if (lines.some((l) => l.includes("Markdown.parse(") || l.includes('md"'))) return "text";
    if (!["let", "begin"].includes(lines[0]) || lines[lines.length - 1] !== "end" || lines.length < 3) return null;
    const last = lines[lines.length - 2];
    return last.split(",").every((n) => /^[\p{L}\p{N}_]+$/u.test(n.trim())) ? last : null;
  }
  function codeLines(code) {
    let inBlock = false;
    return code.split("\n").map((l) => l.trim()).filter((line) => {
      if (inBlock || line.startsWith("#=")) {
        inBlock = !line.endsWith("=#");
        return false;
      }
      return line !== "" && !line.startsWith("#");
    });
  }
  function markdownText(code) {
    const first = codeLines(code)[0];
    if (first === void 0) return null;
    const rest = code.slice(code.indexOf(first));
    const m = rest.match(/^md"(?:"")?/);
    return m ? rest.slice(m[0].length).trimEnd().replace(/"+$/, "") : null;
  }

  // src/state.ts
  var context = { host: "This Mac", agent: "Claude", asking: false, crash: null };

  // src/askbox.ts
  var svg = (body, size = 12) => `<svg width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
  var ICONS = {
    cell: svg(`<rect x="2.5" y="3" width="11" height="10" rx="1.5"></rect><path d="M5 6.5h6M5 9.5h4"></path>`),
    lines: svg(`<path d="M2 4.5h6M2 8h5M2 11.5h6"></path><path d="M11.5 3v10M10 3h3M10 13h3"></path>`),
    text: svg(`<path d="M3 4.5h10M3 8h10M3 11.5h6"></path>`),
    add: svg(`<path d="M8 3.5v9M3.5 8h9"></path>`),
    // Point's own glyph (the board's Pointer icon, Glyph::Pointer in new_session.rs).
    point: svg(`<path d="M3.3 2 3.3 13.3 6.4 10.4 8.8 14.7 10.7 13.7 8.4 9.5 12.7 9.5Z"></path>`),
    none: ""
  };
  var UP = svg(`<path d="M8 13V3.5M4 7.5l4-4 4 4"></path>`, 14);
  var DOWN = svg(`<path d="M4.5 6.25 8 9.75l3.5-3.5"></path>`);
  var QUOTE = svg(`<path d="M3 4.5h3.75v3.75H5c0 1.6-.6 2.6-2 3.25"></path><path d="M9.25 4.5H13v3.75h-1.75c0 1.6-.6 2.6-2 3.25"></path>`, 14);
  var KEYS = mac ? { send: "\u21A9", add: "\u2318\u21A9" } : { send: "Enter", add: "Ctrl+Enter" };
  var WORKING = "endeavorWorking";
  var css = `
  .endeavor-prompt { box-sizing: border-box; display: flex; flex-direction: column; gap: 6px; font: 13px/18px system-ui, sans-serif;
    color: var(--e-text-primary); text-align: left; }
  .endeavor-prompt.popover { position: fixed; z-index: 1000; padding: 7px 8px 8px; border-radius: 10px; border: 1px solid var(--e-popover-edge);
    background: var(--e-popover-bg); box-shadow: 0 12px 32px var(--e-shadow-popover); }
  .endeavor-prompt .head { display: flex; align-items: center; gap: 8px; padding: 0 2px 0 4px; font-size: 12px; line-height: 17px;
    color: var(--e-text-faint); white-space: nowrap; }
  .endeavor-prompt .what { flex: 1; min-width: 0; display: flex; align-items: center; gap: 6px; overflow: hidden; text-overflow: ellipsis; }
  .endeavor-prompt .what svg { flex: none; }
  .endeavor-prompt .what .name { font-family: JuliaMono, ui-monospace, monospace; font-size: 11.5px; color: var(--e-text-secondary); }
  .endeavor-prompt .keys { flex: none; font-size: 11px; }
  .endeavor-prompt .quote { margin: 0 4px; border-left: 2px solid var(--e-control-edge); padding-left: 8px; font-size: 11.5px; line-height: 17px;
    color: var(--e-text-faint); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .endeavor-prompt .quote.code { font-family: JuliaMono, ui-monospace, monospace; }
  .endeavor-prompt .note { padding: 0 4px; font-size: 12px; color: var(--e-text-faint); }
  .endeavor-field { position: relative; display: flex; align-items: flex-end; gap: 4px; min-height: 34px; box-sizing: border-box;
    padding: 4px 4px 4px 9px; border-radius: 6px; border: 1px solid var(--e-control-edge); background: var(--e-bg-page); }
  .endeavor-field:focus-within { border-color: var(--e-focus-ring); }
  .endeavor-field textarea { flex: 1; min-width: 0; resize: none; border: 0; outline: none; background: transparent; padding: 1.5px 0;
    height: 21px; max-height: 126px; overflow-y: auto; color: var(--e-text-primary); font: 14px/21px system-ui, sans-serif; }
  .endeavor-field textarea::placeholder { color: var(--e-text-muted); }
  .endeavor-field .send { flex: none; width: 24px; height: 24px; border: 0; padding: 0; border-radius: 50%; cursor: pointer;
    display: flex; align-items: center; justify-content: center; background: var(--e-bg-raised); color: var(--e-text-faint); }
  .endeavor-field .send.ready { background: var(--e-accent); color: #fff; }
  .endeavor-field .options { flex: none; width: 18px; height: 24px; border: 0; padding: 0; border-radius: 5px; cursor: pointer;
    display: flex; align-items: center; justify-content: center; background: transparent; color: var(--e-text-muted); }
  .endeavor-field .options:hover, .endeavor-field .options[aria-expanded="true"] { background: var(--e-control-edge); color: var(--e-text-primary); }
  .endeavor-field [role="menu"] { position: absolute; right: -6px; top: calc(100% + 6px); z-index: 5; width: ${mac ? 216 : 236}px; box-sizing: border-box;
    display: flex; flex-direction: column; padding: 4px; border-radius: 10px; border: 1px solid var(--e-popover-edge);
    background: var(--e-popover-bg); box-shadow: 0 12px 32px var(--e-shadow-popover); font: 13px system-ui, sans-serif; }
  .endeavor-field [role="menu"][hidden] { display: none; }
  .endeavor-field [role="menuitem"] { height: 28px; flex: none; display: flex; align-items: center; gap: 8px; padding: 0 8px; border: 0; border-radius: 6px;
    background: transparent; color: var(--e-text-primary); font: inherit; cursor: pointer; text-align: left; }
  .endeavor-field [role="menuitem"]:hover { background: var(--e-menu-hover); }
  .endeavor-field [role="menuitem"] svg { color: var(--e-text-muted); }
  .endeavor-field [role="menuitem"] .key { margin-left: auto; padding-left: 12px; font-size: 12px; color: var(--e-text-faint); white-space: nowrap; }
  html:not([data-endeavor-working]) .endeavor-prompt .working, html[data-endeavor-working] .endeavor-prompt .idle { display: none; }
`;
  function askBox(o) {
    const root = document.createElement("div");
    root.className = "endeavor-prompt";
    root.dataset.endeavorUi = "";
    root.setAttribute("role", "dialog");
    root.setAttribute("aria-label", o.label);
    root.innerHTML = `<div class="head"><span class="what"></span><span class="keys"><span class="idle">${KEYS.send} send</span><span class="working">${KEYS.send} queue</span> \xB7 ${KEYS.add} add to message</span></div><div class="endeavor-field"><textarea rows="1" spellcheck="false" autocorrect="off" autocapitalize="off"></textarea><button class="send" aria-label="Send">${UP}</button><button class="options" aria-label="Send options" aria-haspopup="menu" aria-expanded="false">${DOWN}</button><div role="menu" hidden><button role="menuitem" data-add="false">${UP}<span class="idle">Send now</span><span class="working">Send after this turn</span><span class="key">${KEYS.send}</span></button><button role="menuitem" data-add="true">${QUOTE}<span>Add to message</span><span class="key">${KEYS.add}</span></button></div></div><div class="note working">${context.agent} is working. This goes after its turn.</div>`;
    const what = root.querySelector(".what");
    const field = root.querySelector(".endeavor-field");
    const text = root.querySelector("textarea");
    const sendButton = root.querySelector(".send");
    const options = root.querySelector(".options");
    const menu = root.querySelector("[role=menu]");
    text.placeholder = o.placeholder;
    text.setAttribute("aria-label", o.placeholder);
    if (o.quote) {
      const quote = document.createElement("div");
      quote.className = o.quote.code ? "quote code" : "quote";
      quote.textContent = o.quote.text.split(/\s+/).join(" ").trim();
      field.before(quote);
    }
    const showMenu = (shown) => {
      menu.hidden = !shown;
      options.setAttribute("aria-expanded", String(shown));
    };
    const fit = () => {
      sendButton.classList.toggle("ready", !!text.value.trim());
      text.style.height = "21px";
      text.style.height = `${Math.min(text.scrollHeight, 126)}px`;
    };
    text.addEventListener("input", fit);
    text.addEventListener(
      "keydown",
      (e) => {
        e.stopPropagation();
        if (e.key === "Enter" && !e.shiftKey) {
          e.preventDefault();
          showMenu(false);
          o.done(modHeld(e), e);
        }
      },
      true
    );
    for (const button of root.querySelectorAll("button")) button.onmousedown = (e) => e.preventDefault();
    options.onclick = () => showMenu(menu.hidden);
    sendButton.onclick = (e) => o.done(false, e);
    for (const item of menu.querySelectorAll("[role=menuitem]")) {
      item.onclick = (e) => {
        showMenu(false);
        o.done(item.dataset.add === "true", e);
      };
    }
    const setWhat = (icon, name, detail) => {
      what.innerHTML = ICONS[icon];
      if (name) {
        const span = document.createElement("span");
        span.className = "name";
        span.textContent = name;
        what.append(span, Object.assign(document.createElement("span"), { textContent: "\xB7" }));
      }
      what.append(Object.assign(document.createElement("span"), { className: "status", textContent: detail }));
    };
    return { root, text, what, setWhat };
  }
  function initAskBox() {
    const style = document.createElement("style");
    style.textContent = css;
    document.head.append(style);
    on("context", (msg) => {
      if (msg.working) document.documentElement.dataset[WORKING] = "";
      else delete document.documentElement.dataset[WORKING];
    });
  }

  // src/quote.ts
  var SHOOTING = "endeavor-shooting";
  var waiting = /* @__PURE__ */ new Map();
  var nextShot = 1;
  var nextFrame = () => new Promise((done) => requestAnimationFrame(() => done()));
  async function shoot(b) {
    const top = b.top - window.scrollY;
    const bottom = b.bottom - window.scrollY;
    if (top < 0 || bottom > window.innerHeight) window.scrollBy(0, top < 0 || bottom - top > window.innerHeight ? top - 8 : bottom - window.innerHeight + 8);
    document.body.classList.add(SHOOTING);
    await nextFrame();
    await nextFrame();
    const x = Math.max(0, b.left - window.scrollX);
    const y = Math.max(0, b.top - window.scrollY);
    const rect = {
      x,
      y,
      width: Math.min(window.innerWidth, b.right - window.scrollX) - x,
      height: Math.min(window.innerHeight, b.bottom - window.scrollY) - y
    };
    const id = nextShot++;
    await new Promise((done) => {
      waiting.set(id, done);
      setTimeout(done, 3e3);
      send({ type: "shoot", id, rect });
    });
    waiting.delete(id);
    document.body.classList.remove(SHOOTING);
    return id;
  }
  function sendQuote(picks, comment, add) {
    const notebook = new URLSearchParams(location.search).get("id");
    send({ type: "quote", notebook, picks, comment, add });
  }

  // src/annotate.ts
  var css2 = `
  /* Only the hovered elements: a rule over every element in every cell restyles the whole notebook on entry. */
  body.annotating pluto-cell:hover, body.annotating pluto-cell :hover { cursor: crosshair !important; }
  /* docs/ui-spec.md, "Pointing overlay": 25% dim, 1px accent edge, plain-text hint
     pill, dashed hover and solid picked outlines, dashed box for a drawn region. */
  body.annotating .annotate-hover { outline: 1.5px dashed var(--e-hover-edge); outline-offset: 3px; }
  body.annotating .annotate-picked { outline: 1.5px solid var(--e-accent); outline-offset: 3px; }
  body.annotating.annotate-drawing .annotate-hover { outline: none; }
  body.annotating.annotate-drawing, body.annotating.annotate-drawing * { user-select: none; }
  /* Pluto's own gutter numbers, which it shows as dots until hovered. */
  body.annotate-numbers pluto-input .cm-lineNumbers .cm-gutterElement { color: var(--cm-color-line-numbers) !important; }
  body.annotate-numbers pluto-input .cm-lineNumbers .cm-gutterElement::after { color: transparent !important; }
  body.annotating pluto-input .cm-line.annotate-line { background: rgba(204, 63, 0, 0.12); }
  #annotate-tag { position: absolute; z-index: 9999; display: none; pointer-events: none; padding: 0 6px; border-radius: 4px;
    background: var(--e-accent); color: #fff; font: 11px/18px system-ui; }
  body.annotating #annotate-tag.shown { display: block; }
  #annotate-frame { position: fixed; inset: 0; pointer-events: none; z-index: 9999;
    background: var(--e-dim); box-shadow: inset 0 0 0 1px var(--e-accent); display: none; }
  #annotate-box { position: absolute; z-index: 9999; pointer-events: none; display: none;
    border: 1.5px dashed var(--e-accent); border-radius: 2px; }
  body.annotating #annotate-box.shown { display: block; }
  #annotate-hint { position: fixed; top: 12px; left: 50%; transform: translateX(-50%); z-index: 10000;
    display: none; align-items: center; gap: 6px; padding: 4px 12px; border-radius: 999px; border: 1px solid transparent;
    background: var(--e-hint-bg); color: var(--e-hint-text); font: 12px system-ui; white-space: nowrap; }
  #annotate-hint .done { color: var(--e-text-primary); cursor: pointer; }
  #annotate-hint .done:hover { text-decoration: underline; }
  #annotate-bar { position: fixed; left: 50%; bottom: 16px; transform: translateX(-50%);
    z-index: 10000; display: none; flex-direction: column; gap: 6px; box-sizing: border-box;
    width: min(640px, calc(100vw - 32px)); padding: 8px; border-radius: 10px; border: 1px solid transparent;
    background: var(--e-bar-bg); color: var(--e-text-primary); font: 13px system-ui;
    backdrop-filter: blur(14px); -webkit-backdrop-filter: blur(14px);
    box-shadow: 0 8px 30px var(--e-bar-shadow); }
  #annotate-bar.placed { transform: none; bottom: auto; }
  body.annotating #annotate-bar, body.annotating #annotate-frame, body.annotating #annotate-hint { display: flex; }
  @media (prefers-color-scheme: light) {
    #annotate-hint, #annotate-bar { border-color: var(--e-popover-edge); }
    #annotate-hint { box-shadow: 0 12px 32px var(--e-shadow-popover); }
  }
  /* While the app takes a picture, the notebook shows as it is. */
  body.annotating.${SHOOTING} #annotate-frame, body.annotating.${SHOOTING} #annotate-box, body.annotating.${SHOOTING} #annotate-tag,
  body.annotating.${SHOOTING} #annotate-hint, body.annotating.${SHOOTING} #annotate-bar { display: none; }
  body.annotating.${SHOOTING} .annotate-picked, body.annotating.${SHOOTING} .annotate-hover { outline: none; }
  body.annotating.${SHOOTING} pluto-input .cm-line.annotate-line { background: none; }
`;
  var DRAG = 4;
  var FIGURE = "img, svg, canvas, video";
  var PARAGRAPH = "p, li, h1, h2, h3, h4, h5, h6, pre, blockquote, table";
  function targetAt(el) {
    const cell = el?.closest("pluto-cell");
    if (!el || !cell) return null;
    const input = el.closest("pluto-input");
    if (input) return { part: "code", cell, el: input };
    const output = el.closest("pluto-output");
    if (!output) return { part: "cell", cell, el: cell };
    let figure = el.closest(FIGURE);
    while (figure?.parentElement?.closest(FIGURE) && output.contains(figure.parentElement.closest(FIGURE))) figure = figure.parentElement.closest(FIGURE);
    if (figure && output.contains(figure)) return { part: "figure", cell, el: figure };
    const paragraph = el.closest(PARAGRAPH);
    if (paragraph && output.contains(paragraph)) return { part: "output", cell, el: paragraph };
    return { part: output.querySelector(FIGURE) ? "figure" : "output", cell, el: output };
  }
  var pageBox = (r) => ({
    left: r.left + window.scrollX,
    top: r.top + window.scrollY,
    right: r.right + window.scrollX,
    bottom: r.bottom + window.scrollY
  });
  var cmLines = (cell) => [...cell.querySelectorAll("pluto-input .cm-line")];
  function pickStatus(picks) {
    if (picks.length > 1) return `${picks.length} picks`;
    const pick = picks[0];
    if (!pick) return "Click something to pick it";
    if (pick.part === "box") return `Box over ${pick.cells.length} cell${pick.cells.length === 1 ? "" : "s"}`;
    const name = cellName(pick.code);
    switch (pick.part) {
      case "cell":
        return `Cell ${name}`;
      case "lines":
        return pick.lines[0] === pick.lines[1] ? `Line ${pick.lines[0]} of ${name}` : `Lines ${pick.lines[0]}\u2013${pick.lines[1]} of ${name}`;
      case "output":
        return `Output of ${name}`;
      case "figure":
        return `Figure in ${name}`;
    }
  }
  function samePick(a, b) {
    if (a.box || b.box) return a === b;
    if (a.pick.part === "lines" && b.pick.part === "lines") return a.pick.cell === b.pick.cell && a.pick.lines.join() === b.pick.lines.join();
    return !!a.el && a.el === b.el;
  }
  var state = null;
  function initAnnotate() {
    const picks = [];
    let hover = null;
    const active = () => document.body.classList.contains("annotating");
    const cells = () => [...document.querySelectorAll("pluto-cell")];
    const style = document.createElement("style");
    style.textContent = css2;
    const frame = document.createElement("div");
    frame.id = "annotate-frame";
    const box = document.createElement("div");
    box.id = "annotate-box";
    const tag = document.createElement("div");
    tag.id = "annotate-tag";
    const hint = document.createElement("div");
    hint.id = "annotate-hint";
    hint.innerHTML = `<span>Click to pick \xB7 drag over code lines \xB7 drag elsewhere for a box</span><span>\xB7</span><span class="done" role="button">Done</span>`;
    const bar = document.createElement("div");
    bar.id = "annotate-bar";
    const field = askBox({ label: `Comment for ${context.agent}`, placeholder: `Comment for ${context.agent}\u2026`, done: (add, e) => byUser(e) && sendComment(add) });
    const text = field.text;
    bar.append(field.root);
    document.head.append(style);
    document.body.append(frame, box, tag, hint, bar);
    state = { picks, status: () => active() ? field.what.textContent ?? "" : "", comment: () => text.value };
    on("context", () => {
      const label = `Comment for ${context.agent}`;
      field.root.setAttribute("aria-label", label);
      text.placeholder = `${label}\u2026`;
      text.setAttribute("aria-label", text.placeholder);
    });
    function drawBox(b) {
      box.classList.toggle("shown", !!b);
      if (!b) return;
      Object.assign(box.style, { left: `${b.left}px`, top: `${b.top}px`, width: `${b.right - b.left}px`, height: `${b.bottom - b.top}px` });
    }
    function lastRect() {
      const last = picks.at(-1);
      if (!last) return null;
      if (last.box) {
        const b = last.box;
        return { left: b.left - window.scrollX, top: b.top - window.scrollY, right: b.right - window.scrollX, bottom: b.bottom - window.scrollY };
      }
      const shown = last.lines?.length ? last.lines : last.el ? [last.el] : [];
      if (!shown.length) return null;
      const [first, end] = [shown[0].getBoundingClientRect(), shown[shown.length - 1].getBoundingClientRect()];
      return { left: first.left, top: first.top, right: Math.max(first.right, end.right), bottom: end.bottom };
    }
    function place() {
      const rect = lastRect();
      bar.classList.toggle("placed", !!rect);
      if (!rect) {
        bar.removeAttribute("style");
        return;
      }
      const at = barPlace(rect, window.innerWidth, window.innerHeight, bar.offsetHeight);
      Object.assign(bar.style, { left: `${at.left}px`, top: `${at.top}px`, width: `${at.width}px` });
    }
    function showHover(target) {
      hover?.el.classList.remove("annotate-hover");
      hover = target;
      tag.classList.toggle("shown", !!target);
      if (!target) return;
      target.el.classList.add("annotate-hover");
      tag.textContent = { cell: "Whole cell", code: "Code", output: "Text", figure: "Figure" }[target.part];
      const r = target.el.getBoundingClientRect();
      Object.assign(tag.style, { left: `${r.left + window.scrollX}px`, top: `${r.top + window.scrollY - 22}px` });
    }
    function refresh() {
      for (const el of document.querySelectorAll(".annotate-picked")) el.classList.remove("annotate-picked");
      for (const el of document.querySelectorAll(".annotate-line")) el.classList.remove("annotate-line");
      for (const p of picks) {
        p.el?.classList.add("annotate-picked");
        for (const line of p.lines ?? []) line.classList.add("annotate-line");
      }
      drawBox([...picks].reverse().find((p) => p.box)?.box ?? null);
      field.setWhat("point", "", pickStatus(picks.map((p) => p.pick)));
      place();
    }
    function choose(next, add) {
      const at = picks.findIndex((p) => samePick(p, next));
      if (add) at >= 0 ? picks.splice(at, 1) : picks.push(next);
      else picks.splice(0, picks.length, ...at >= 0 && picks.length === 1 ? [] : [next]);
      refresh();
      text.focus();
    }
    function targetPick(target) {
      const { cell, el } = target;
      const code = cellCode(cell);
      switch (target.part) {
        case "cell":
          return { pick: { part: "cell", cell: cell.id, code }, el };
        case "code": {
          const lines = code.split("\n").length;
          return { pick: { part: "lines", cell: cell.id, code, lines: [1, lines], text: code }, el };
        }
        case "output":
          return { pick: { part: "output", cell: cell.id, code, text: (el.innerText ?? el.textContent ?? "").trim() }, el };
        case "figure":
          return { pick: { part: "figure", cell: cell.id, code }, el };
      }
    }
    function linesPick(cell, from, to) {
      const [first, last] = [Math.min(from, to), Math.max(from, to)];
      const code = cellCode(cell);
      const text2 = code.split("\n").slice(first - 1, last).join("\n");
      return { pick: { part: "lines", cell: cell.id, code, lines: [first, last], text: text2 }, lines: cmLines(cell).slice(first - 1, last) };
    }
    function set(enable) {
      if (enable === active()) return;
      document.body.classList.toggle("annotating", enable);
      document.body.classList.remove("annotate-drawing");
      picks.length = 0;
      drag = null;
      showHover(null);
      if (enable) {
        for (const c of document.querySelectorAll("pluto-cell.selected")) picks.push(targetPick({ part: "cell", cell: c, el: c }));
        text.focus();
      }
      refresh();
      send({ type: "mode", on: enable });
      requestAnimationFrame(() => setTimeout(() => document.body.classList.toggle("annotate-numbers", active())));
    }
    async function sendComment(add) {
      if (!picks.length) return;
      const sending = picks.splice(0, picks.length);
      const comment = text.value.trim();
      text.value = "";
      text.dispatchEvent(new Event("input"));
      refresh();
      const out = [];
      for (const p of sending) {
        if (p.pick.part === "box" || p.pick.part === "figure") {
          const b = p.box ?? (p.el ? pageBox(p.el.getBoundingClientRect()) : null);
          out.push(b ? { ...p.pick, shot: await shoot(b) } : p.pick);
        } else out.push(p.pick);
      }
      sendQuote(out, comment, add);
    }
    let drag = null;
    let justDragged = false;
    const dragBox = (e) => {
      const [x, y] = [e.clientX + window.scrollX, e.clientY + window.scrollY];
      return { left: Math.min(drag.x, x), top: Math.min(drag.y, y), right: Math.max(drag.x, x), bottom: Math.max(drag.y, y) };
    };
    const lineAt = (el, cell) => {
      const line = el?.closest(".cm-line");
      return line && cell.contains(line) ? cmLines(cell).indexOf(line) + 1 : 0;
    };
    const ours = (target) => bar.contains(target) || hint.contains(target);
    const swallow = (e) => {
      const target = e.target;
      if (!active() || ours(target)) return;
      if (e.type !== "pointerdown") e.preventDefault();
      e.stopPropagation();
      const m = e;
      if (e.type === "mousedown" && m.button === 0) {
        const cell = target.closest("pluto-cell");
        const line = cell && !m.altKey ? lineAt(target, cell) : 0;
        drag = { x: m.clientX + window.scrollX, y: m.clientY + window.scrollY, moved: false, add: m.shiftKey, lines: line ? { cell, from: line, to: line } : void 0 };
      }
      if (e.type !== "click") return;
      if (justDragged) {
        justDragged = false;
        return;
      }
      const found = targetAt(target);
      if (found) choose(targetPick(found), m.shiftKey);
    };
    for (const type of ["pointerdown", "mousedown", "click"]) document.addEventListener(type, swallow, true);
    document.addEventListener(
      "mousemove",
      (e) => {
        if (!active()) return;
        if (!drag) return showHover(ours(e.target) ? null : targetAt(e.target));
        if (!drag.moved && Math.hypot(e.clientX + window.scrollX - drag.x, e.clientY + window.scrollY - drag.y) < DRAG) return;
        drag.moved = true;
        e.preventDefault();
        showHover(null);
        if (drag.lines) {
          const line = lineAt(e.target, drag.lines.cell);
          if (line) drag.lines.to = line;
          const shown = linesPick(drag.lines.cell, drag.lines.from, drag.lines.to);
          for (const el of document.querySelectorAll(".annotate-line")) el.classList.remove("annotate-line");
          for (const el of shown.lines ?? []) el.classList.add("annotate-line");
          return;
        }
        document.body.classList.add("annotate-drawing");
        drawBox(dragBox(e));
      },
      true
    );
    document.addEventListener(
      "mouseup",
      (e) => {
        if (!drag) return;
        const done = drag;
        const drawn = done.moved && !done.lines ? dragBox(e) : null;
        drag = null;
        document.body.classList.remove("annotate-drawing");
        if (!done.moved || !active()) return;
        justDragged = true;
        setTimeout(() => justDragged = false, 0);
        if (done.lines) return choose(linesPick(done.lines.cell, done.lines.from, done.lines.to), done.add);
        const under = cells().filter((c) => {
          const r = pageBox(c.getBoundingClientRect());
          return drawn.left < r.right && r.left < drawn.right && drawn.top < r.bottom && r.top < drawn.bottom;
        });
        choose({ pick: { part: "box", cells: under.map((c) => c.id) }, box: drawn }, done.add);
      },
      true
    );
    window.addEventListener("scroll", () => active() && place(), true);
    window.addEventListener("resize", () => active() && place());
    window.addEventListener(
      "keydown",
      (e) => {
        if (e.key === "Escape" && active()) {
          e.preventDefault();
          e.stopPropagation();
          return set(false);
        }
      },
      true
    );
    hint.querySelector(".done").onclick = () => set(false);
    on("annotate", (msg) => set(msg.on));
  }

  // src/engine.ts
  var onEmber = () => location.pathname.startsWith("/ember/");

  // src/theme.ts
  var tokens = `
:root {
  --e-bg-sidebar: #111113;
  --e-bg-page: #151517;
  --e-bg-card: #1C1C1F;
  --e-bg-raised: #26262A;
  --e-bg-sunken: #18181A;
  --e-bg-tag: #222225;
  --e-text-tag: #9A9997;
  --e-border: #2A2A2E;
  --e-control-edge: #3A3A40;
  --e-divider: #1F1F22;
  --e-text-primary: #F0EFEC;
  --e-text-secondary: #BDBCBA;
  --e-text-muted: #8C8B8A;
  --e-text-faint: #858483;
  --e-text-section: #888786;
  --e-accent: #CC3F00;
  --e-accent-text: #E08A5E;
  --e-focus-ring: #E08A5E;
  --e-diff-add: #6CC784;
  --e-diff-del: #E07A7A;
  --e-diff-add-tint: rgba(108, 199, 132, 0.12);
  --e-diff-del-tint: rgba(224, 122, 122, 0.12);
  --e-popover-bg: #26262A;
  --e-popover-edge: #3A3A40;
  --e-menu-hover: #313136;
  --e-shadow-popover: rgba(0, 0, 0, 0.45);
  --e-button-hover: #2E2E33;
  --e-dialog-bg: #1C1C1F;
  --e-dialog-edge: #2A2A2E;
  --e-dialog-shadow: rgba(0, 0, 0, 0.5);

  /* Colours used by one or a few modules that don't match one of the tokens
     above closely enough to reuse it without shifting the dark look. */
  --e-text-dim: #7A7978;
  --e-text-waiting: #5E5E5E;
  --e-text-code: #D4D4D4;
  --e-dot-waiting: #4A4A4E;
  --e-danger: #E07A7A;
  --e-danger-edge: rgba(224, 122, 122, 0.4);
  --e-danger-bg: rgba(224, 122, 122, 0.06);
  --e-diff-add-ch: rgba(108, 199, 132, 0.28);
  --e-diff-del-ch: rgba(224, 122, 122, 0.28);
  --e-you-stripe: #9A9A9A;
  --e-stripe-tint: rgba(204, 63, 0, 0.3);
  --e-hover-edge: #FF7A40;
  --e-dim: rgba(0, 0, 0, 0.25);
  --e-hint-bg: rgba(28, 28, 30, 0.85);
  --e-hint-text: #ccc;
  --e-bar-bg: rgba(28, 28, 30, 0.72);
  --e-bar-shadow: rgba(0, 0, 0, 0.4);
  --e-pill-bg: #1C1C1E;
  --e-pill-edge: #333333;
  --e-prompt-hover: #FF9A6B;
  --e-backdrop: rgba(0, 0, 0, 0.45);
}
@media (prefers-color-scheme: light) {
  :root {
    --e-bg-sidebar: #F3F3F5;
    --e-bg-page: #FCFCFD;
    --e-bg-card: #F4F4F6;
    --e-bg-raised: #EAEAED;
    --e-bg-sunken: #F8F8FA;
    --e-bg-tag: #ECECEF;
    --e-text-tag: #5C5C64;
    --e-border: #E1E1E6;
    --e-control-edge: #8A8A92;
    --e-divider: #E6E6EA;
    --e-text-primary: #1B1B1F;
    --e-text-secondary: #45454C;
    --e-text-muted: #5C5C64;
    --e-text-faint: #66666E;
    --e-text-section: #6B6B73;
    --e-accent: #CC3F00;
    --e-accent-text: #B23600;
    --e-focus-ring: #CC3F00;
    --e-diff-add: #1C7038;
    --e-diff-del: #B42A36;
    --e-diff-add-tint: rgba(28, 140, 70, 0.12);
    --e-diff-del-tint: rgba(200, 40, 60, 0.1);
    --e-popover-bg: #FFFFFF;
    --e-popover-edge: #D9D9DF;
    --e-menu-hover: #EEEEF1;
    --e-shadow-popover: rgba(20, 20, 30, 0.14);
    --e-button-hover: #E2E2E6;
    --e-dialog-bg: #FFFFFF;
    --e-dialog-edge: #D9D9DF;
    --e-dialog-shadow: rgba(20, 20, 30, 0.16);

    --e-text-dim: #66666E;
    --e-text-waiting: #707078;
    --e-text-code: #26262B;
    --e-dot-waiting: #A6A6AE;
    --e-danger: #B42A36;
    --e-danger-edge: rgba(180, 42, 54, 0.35);
    --e-danger-bg: rgba(180, 42, 54, 0.05);
    --e-diff-add-ch: rgba(28, 140, 70, 0.24);
    --e-diff-del-ch: rgba(200, 40, 60, 0.2);
    --e-you-stripe: #8A8A92;
    --e-stripe-tint: rgba(204, 63, 0, 0.25);
    --e-hover-edge: #CC3F00;
    --e-dim: rgba(20, 20, 30, 0.1);
    --e-hint-bg: rgba(255, 255, 255, 0.94);
    --e-hint-text: #45454C;
    --e-bar-bg: rgba(255, 255, 255, 0.94);
    --e-bar-shadow: rgba(20, 20, 30, 0.14);
    --e-pill-bg: #F4F4F6;
    --e-pill-edge: #D2D2D8;
    --e-prompt-hover: #B23600;
    --e-backdrop: rgba(20, 20, 30, 0.35);
  }
}
`;
  var endeavorLook = `
@media (prefers-color-scheme: dark) {
  :root {
    --main-bg-color: #151517;
    --header-bg-color: #151517;
    --footer-bg-color: #151517;
    --rule-color: rgba(255, 255, 255, 0.08);
    --code-background: #1B1B1E;
    --normal-cell-color: rgba(100, 100, 100, 0.18);
    --dark-normal-cell-color: rgba(100, 100, 100, 0.3);
    --code-differs-cell-color: #9A9A9A;
    --selected-cell-color: rgba(143, 170, 216, 0.45);
    --pluto-output-color: #BDBCBA;
    --pluto-output-h-color: #E0DFDC;
    --pluto-output-bg-color: #151517;
    --pluto-runarea-bg-color: #1C1C1F;
    --pluto-logs-bg-color: #1C1C1F;
    --overlay-button-bg: #1C1C1F;
    --input-context-menu-bg-color: #1C1C1F;
    --input-context-menu-border-color: #2A2A2E;
    --cm-selection-background: rgba(143, 170, 216, 0.3);
    --cm-color-editor-text: #D4D4D4;
    --cm-color-keyword: #D98BB5;
    --cm-color-control-operator: #D98BB5;
    --cm-color-literal: #CFA47A;
    --cm-color-symbol: #CFA47A;
    --cm-color-string: #A3C48C;
    --cm-color-function: #9AB6E6;
    --cm-color-builtin: #9AB6E6;
    --cm-color-comment: #858585;
    --cm-color-line-numbers: #555555;
    /* Live docs */
    --helpbox-bg-color: #1C1C1F;
    --helpbox-header-bg-color: #26262A;
    --helpbox-header-tab-bg-color: #26262A;
    --helpbox-header-color: #F0EFEC;
    --helpbox-text-color: #D4D4D4;
    --helpbox-search-bg-color: #151517;
    --helpbox-search-border-color: #3A3A40;
    --helpbox-box-shadow-color: rgba(0, 0, 0, 0.3);
    --docs-binding-bg: #26262A;
    /* Frontmatter and Pluto's other dialogs */
    --export-bg-color: #1C1C1F;
    --export-color: #D4D4D4;
    --frontmatter-button-bg-color: #26262A;
    --frontmatter-input-bg-color: #151517;
    --frontmatter-input-border-color: #3A3A40;
    /* A cell's run time: faint text, no chip */
    --pluto-runarea-bg-color: transparent;
    --pluto-runarea-span-color: #858483;
  }
  .pluto-modal { border: 1px solid #2A2A2E; border-radius: 8px !important; box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5) !important; }
  .pluto-modal-dark h1 { color: #F0EFEC; font-weight: 600; }
  body.presentation nav#slide_controls { gap: 4px; padding: 4px; margin: 12px; border-radius: 8px;
    background: #1C1C1F; border: 1px solid #2A2A2E; }
  nav#slide_controls > button { border-radius: 5px; opacity: 0.8; }
  nav#slide_controls > button:hover { background: #26262A; opacity: 1; }
}
@media (prefers-color-scheme: light) {
  :root {
    --main-bg-color: #FCFCFD;
    --header-bg-color: #FCFCFD;
    --footer-bg-color: #FCFCFD;
    --rule-color: rgba(20, 20, 30, 0.08);
    --code-background: #F4F4F6;
    --normal-cell-color: rgba(120, 120, 130, 0.2);
    --dark-normal-cell-color: rgba(120, 120, 130, 0.34);
    --code-differs-cell-color: #8A8A92;
    --selected-cell-color: rgba(64, 112, 196, 0.22);
    --pluto-output-color: #45454C;
    --pluto-output-h-color: #1B1B1F;
    --pluto-output-bg-color: #FCFCFD;
    --pluto-runarea-bg-color: #F4F4F6;
    --pluto-logs-bg-color: #F4F4F6;
    --overlay-button-bg: #F4F4F6;
    --input-context-menu-bg-color: #FFFFFF;
    --input-context-menu-border-color: #D9D9DF;
    --cm-selection-background: rgba(64, 112, 196, 0.18);
    --cm-color-editor-text: #26262B;
    --cm-color-keyword: #A2366F;
    --cm-color-control-operator: #A2366F;
    --cm-color-literal: #955A12;
    --cm-color-symbol: #955A12;
    --cm-color-string: #3F7A22;
    --cm-color-function: #2F5FA6;
    --cm-color-builtin: #2F5FA6;
    --cm-color-comment: #6B6B73;
    --cm-color-line-numbers: #6E6E76;
    /* Live docs */
    --helpbox-bg-color: #F4F4F6;
    --helpbox-header-bg-color: #EAEAED;
    --helpbox-header-tab-bg-color: #EAEAED;
    --helpbox-header-color: #1B1B1F;
    --helpbox-text-color: #26262B;
    --helpbox-search-bg-color: #FCFCFD;
    --helpbox-search-border-color: #8A8A92;
    --helpbox-box-shadow-color: rgba(20, 20, 30, 0.12);
    --docs-binding-bg: #EAEAED;
    /* Frontmatter and Pluto's other dialogs */
    --export-bg-color: #FFFFFF;
    --export-color: #26262B;
    --frontmatter-button-bg-color: #EAEAED;
    --frontmatter-input-bg-color: #FCFCFD;
    --frontmatter-input-border-color: #8A8A92;
    /* A cell's run time: faint text, no chip */
    --pluto-runarea-bg-color: transparent;
    --pluto-runarea-span-color: #6B6B73;
  }
  .pluto-modal { border: 1px solid #D9D9DF; border-radius: 8px !important; box-shadow: 0 12px 40px rgba(20, 20, 30, 0.16) !important; }
  .pluto-modal h1 { color: #1B1B1F; font-weight: 600; }
  body.presentation nav#slide_controls { gap: 4px; padding: 4px; margin: 12px; border-radius: 8px;
    background: #FFFFFF; border: 1px solid #D9D9DF; }
  nav#slide_controls > button { border-radius: 5px; opacity: 0.8; }
  nav#slide_controls > button:hover { background: #EAEAED; opacity: 1; }
}
`;
  var layout = `
/* The cells' side gap is the chat column's margin (main.rs, chat_margin): 50px,
   shrinking with the pane's width (50 per 860) at narrow widths, but never under
   28px, which Pluto's shoulder (the show/hide-code eye, the status bar) needs
   left of a cell. main's own padding (25px left, 6px right) is part of that gap,
   less the 5px a cell's box reaches left of its edge.
   Left-aligned, not centred (Pluto classic stays centred): a floated PlutoUI
   TableOfContents sits at the right, and centring put it over the notebook
   rather than beside it. 731px keeps the reading width Pluto centred at; while
   the pane is narrower than that, the gaps on both sides are equal. Pluto's own
   rule sizes main at width: 100%, so the margins come out of that width.
   The overview rail (rail.ts) is fixed to the right of the viewport, not the
   notebook, so it isn't affected either way. */
pluto-editor { --e-side: clamp(28px, calc(100vw * 50 / 860), 50px); }
pluto-editor main { margin-left: calc(var(--e-side) - 20px) !important; margin-right: auto !important;
  width: calc(100% - 2 * var(--e-side) + 20px + 6px) !important; max-width: 731px !important; }
/* The preamble only holds Pluto's sticky "Save all changes" button; its 25px
   strip above the first cell is gone, and the button floats over the top. */
pluto-editor main > preamble { height: 0 !important; margin-top: 0 !important; overflow: visible; }
pluto-runarea > span { font-size: 10px; }
`;
  var appChrome = `
header#pluto-nav, footer { display: none !important; }
/* Not display: none \u2014 Pluto alerts "window too small to show docs" whenever it opens a panel it finds undisplayed. */
/* Pluto shows the panel only from 500px wide; a narrower notebook pane would alert on every panel it opens. */
#helpbox-wrapper { display: block !important; }
html:not([data-endeavor-drawer="docs"]) #helpbox-wrapper { visibility: hidden !important; pointer-events: none !important;
  position: fixed !important; width: 0 !important; height: 0 !important; overflow: hidden !important; }
.outline-frame.safe-preview, .outline-frame-actions-container.safe-preview { display: none !important; }
pluto-output.rich_output:has(> .safe-preview-output) { display: none !important; }
pluto-editor > main { padding-top: 16px; }
/* The web view draws over native views, so the notebook header can't blur
   what's under it (docs/ui-spec.md, "Fades, not blur").
   Instead, the top of the page fades into the page background, so a cell
   scrolled toward the top softens instead of cutting off hard. Fixed to the
   viewport, not the scroller, and never over the first cell at rest: the
   16px top padding above leaves exactly this fade's height clear.
   pointer-events: none keeps clicks and selection reaching the page under it. */
body::before {
  content: "";
  position: fixed;
  top: 0;
  left: 0;
  right: 0;
  height: 16px;
  background: linear-gradient(to bottom, var(--main-bg-color), transparent);
  pointer-events: none;
  z-index: 50;
}
`;
  var classic = `
nav#at_the_top > pluto-filepicker, nav#at_the_top > div.desktop_picker_group, #ember-file-name { display: none !important; }
`;
  var emberLook = `
.ember-settings-section:has(#ember-setting-theme) { display: none !important; }
#ember-safe-preview { display: none !important; }
`;
  function syncTheme() {
    const dark = window.matchMedia("(prefers-color-scheme: dark)");
    const root = document.documentElement;
    const sync = () => {
      if (root.dataset.endeavorLook !== "endeavor") return;
      const want = dark.matches ? "dark" : "light";
      if (root.getAttribute("data-theme") !== want) root.setAttribute("data-theme", want);
    };
    dark.addEventListener("change", sync);
    new MutationObserver(sync).observe(root, { attributes: true, attributeFilter: ["data-theme", "data-endeavor-look"] });
    sync();
  }
  var both = `
footer form#feedback { display: none !important; }
`;
  var juliaMono = [
    { file: "Regular", weight: 400, style: "normal" },
    { file: "Bold", weight: 700, style: "normal" },
    { file: "RegularItalic", weight: 400, style: "italic" }
  ].map(
    ({ file, weight, style }) => `
@font-face {
  font-family: JuliaMono;
  src: url("endeavor://localhost/fonts/JuliaMono-${file}.ttf") format("truetype");
  font-display: swap;
  font-weight: ${weight};
  font-style: ${style};
}`
  ).join("");
  function initTheme() {
    const tokenSheet = document.createElement("style");
    tokenSheet.id = "endeavor-tokens";
    tokenSheet.textContent = tokens;
    document.head.append(tokenSheet);
    const fonts = document.createElement("style");
    fonts.id = "endeavor-fonts";
    fonts.textContent = juliaMono;
    document.head.append(fonts);
    const style = document.createElement("style");
    style.id = "endeavor-theme";
    document.head.append(style);
    on("theme", (msg) => {
      const endeavor = msg.name === "endeavor";
      style.textContent = both + (endeavor ? endeavorLook + layout + appChrome + (onEmber() ? emberLook : "") : msg.name === "view" ? endeavorLook + layout : classic);
      document.documentElement.dataset.endeavorLook = msg.name === "view" ? "endeavor" : msg.name;
    });
    if (onEmber()) syncTheme();
  }

  // src/view.ts
  var FORMAT = "A quote's uri, notebook://pluto/{notebook_id}/cell/{cell_id}, names its cell (it is not a fetchable URL); `lines` are line numbers in the cell's code, counting from 1. `part` says what was quoted when it isn't code: `output` (text from the cell's output), `figure` (the cell's output, a picture) or `box` (a box the user drew over the notebook; `uri` lists the cells under it). No picture comes with a figure or box here: read current code and outputs with the notebook MCP tools.";
  var QUOTE_NOTE = "[Endeavor] The user quoted parts of the notebook in this message, each as a <quote> followed by their comment on it. " + FORMAT;
  var uri = (notebook, cell) => `notebook://pluto/${notebook}/cell/${cell}`;
  function quoteBlock(notebook, pick) {
    if (pick.part === "box") return `<quote part="box" uri="${pick.cells.map((c) => uri(notebook, c)).join(" ")}"/>`;
    const at = `cell="${cellName(pick.code).replace(/"/g, "'")}"`;
    const where = uri(notebook, pick.cell);
    switch (pick.part) {
      case "cell":
        return `<quote ${at} uri="${where}">
${pick.code}
</quote>`;
      case "lines":
        return `<quote ${at} lines="${pick.lines[0]}-${pick.lines[1]}" uri="${where}">
${pick.text}
</quote>`;
      case "output":
        return `<quote ${at} part="output" uri="${where}">
${pick.text}
</quote>`;
      case "figure":
        return `<quote ${at} part="figure" uri="${where}"/>`;
    }
  }
  function quoteText(notebook, picks, comment) {
    const blocks = picks.map((p) => quoteBlock(notebook, p));
    return comment ? `${blocks.join("\n\n")}
${comment}` : blocks.join("\n\n");
  }
  var css3 = `
  #endeavor-point { position: fixed; right: 16px; bottom: 16px; z-index: 9998; display: flex; align-items: center; gap: 6px;
    height: 28px; padding: 0 12px; border-radius: 999px; border: 1px solid var(--e-popover-edge);
    background: var(--e-popover-bg); color: var(--e-text-secondary); font: 12px system-ui, -apple-system, "Segoe UI", sans-serif;
    box-shadow: 0 4px 16px var(--e-shadow-popover); cursor: pointer; }
  #endeavor-point:hover { color: var(--e-text-primary); background: var(--e-button-hover); }
  #endeavor-point .key { color: var(--e-text-faint); }
  body.annotating #endeavor-point { display: none; }
  #endeavor-sent { position: fixed; right: 16px; bottom: 16px; z-index: 10001; display: none;
    padding: 4px 12px; border-radius: 999px; background: var(--e-hint-bg); color: var(--e-hint-text); font: 12px system-ui; }
  #endeavor-sent.shown { display: block; }
`;
  var started = false;
  function start(notebook, host2) {
    if (started) return;
    started = true;
    const added = [];
    let toPage = null;
    const sent = document.createElement("div");
    sent.id = "endeavor-sent";
    let hide = 0;
    const say = (text) => {
      sent.textContent = text;
      sent.classList.add("shown");
      clearTimeout(hide);
      hide = window.setTimeout(() => sent.classList.remove("shown"), 4e3);
    };
    const kept = (why) => say(`${why} Your ${added.length === 1 ? "pick is" : "picks are"} kept: ${mac ? "\u21A9" : "Enter"} with your next pick passes ${added.length === 1 ? "it" : "them"} to the chat.`);
    setHost((msg) => {
      if (msg.type === "shoot") return setTimeout(() => toPage?.({ type: "shot", id: msg.id }));
      if (msg.type !== "quote" || !msg.picks.length) return;
      const text = quoteText(notebook, msg.picks, msg.comment);
      if (msg.add) {
        added.push(text);
        return say(`${added.length} added. ${mac ? "\u21A9" : "Enter"} passes ${added.length === 1 ? "it" : "them"} to the chat with the next pick.`);
      }
      const earlier = added.splice(0);
      host2.send(`${QUOTE_NOTE}

${[...earlier, text].join("\n\n")}`).then(
        () => say("Passed to the chat"),
        (e) => {
          added.unshift(...earlier, text);
          kept(`Couldn't reach Claude's chat${e instanceof Error && e.message ? `: ${e.message}` : ""}.`);
        }
      );
    });
    initTheme();
    const receive = (msg) => window.__endeavor?.receive(msg);
    toPage = receive;
    receive({ type: "theme", name: "view" });
    if (!host2.canSend) return;
    initAskBox();
    initAnnotate();
    const style = document.createElement("style");
    style.textContent = css3;
    document.head.append(style);
    const button = document.createElement("button");
    button.id = "endeavor-point";
    button.type = "button";
    button.title = "Point at a cell, its code or its output, and comment on it for Claude";
    button.innerHTML = `<span>Point</span><span class="key">${mac ? "\u2318\u21E7E" : "Ctrl+Shift+E"}</span>`;
    const point = (on2) => receive({ type: "annotate", on: on2 });
    button.addEventListener("click", (e) => e.isTrusted && point(true));
    document.body.append(button, sent);
    window.addEventListener(
      "keydown",
      (e) => {
        if (!e.isTrusted || !modHeld(e) || !e.shiftKey || e.key.toLowerCase() !== "e" || mac && e.ctrlKey) return;
        e.preventDefault();
        e.stopPropagation();
        point(!document.body.classList.contains("annotating"));
      },
      true
    );
  }
  window.EndeavorView = { start };
})();
