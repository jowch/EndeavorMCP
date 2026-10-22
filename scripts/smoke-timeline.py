#!/usr/bin/env python3
"""How a smoke attempt's context filled, request by request, and what filled it.

usage: scripts/smoke-timeline.py ATTEMPT_DIR [--top N] [--csv OUT]   (needs numpy)

ATTEMPT_DIR is a smoke run's <task>/attempt-<n> folder (docs/smoke.md). From
Claude's transcript it prints, for each user message, the requests it took and
the context at its first and last; then what is in the context at the end, by
source; then the largest single items.

When Claude Code compacted the conversation, the earlier part is gone from
the context: the fit leaves out the step across each compaction, the table
of the end context counts only what came after the last one, and an item
before it counts as re-read only until it.

The context of each request comes from Claude's own usage. What filled it is
estimated: the growth between two requests is the agent's last message (the
code and text it wrote, and its thinking, which the transcript holds only as a
signature) plus what came back (tool replies, pictures). A least-squares fit
over all requests gives tokens per character for each kind, and per picture.
A message's own usage can't be used for this: its output count stops at its
first part.
"""
import collections, json, os, sys

import numpy as np

def source(name, inp):
    if name == "Read" and "/skills/" in str(inp.get("file_path", "")):
        return "skill reference: " + os.path.basename(inp["file_path"])
    if name == "Read":
        return "Read: " + os.path.basename(str(inp.get("file_path", "")))
    if name.startswith("mcp__"):
        return name.split("__")[-1]
    return {"Skill": "skill text", "ToolSearch": "tool schemas"}.get(name, name)

KINDS = ("written", "thinking", "reply", "picture")

def main():
    attempt = sys.argv[1]
    top = int(sys.argv[sys.argv.index("--top") + 1]) if "--top" in sys.argv else 15
    names = {}
    reqs = []  # turn, ctx, items: what this request added for the next [(kind, source, size, label)]
    turn, seen = 1, None  # each user message ends with a result event
    compacted = []  # the number of requests made before each compaction
    for line in open(os.path.join(attempt, "transcript.jsonl")):
        try:
            e = json.loads(line)
        except ValueError:
            continue
        t = e.get("type")
        if t == "result":
            turn += 1
        elif t == "system" and e.get("subtype") == "compact_boundary":
            compacted.append(len(reqs))
        elif t == "assistant":
            m = e["message"]
            if m["id"] != seen:
                seen = m["id"]
                u = m.get("usage", {})
                ctx = sum(u.get(k, 0) for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))
                reqs.append({"turn": turn, "ctx": ctx, "items": []})
            for c in m["content"]:
                if c.get("type") == "tool_use":
                    src = source(c["name"], c["input"])
                    names[c["id"]] = (src, json.dumps(c["input"])[:80])
                    reqs[-1]["items"].append(("written", "agent's calls: " + src, len(json.dumps(c["input"])), ""))
                elif c.get("type") == "text":
                    reqs[-1]["items"].append(("written", "agent's text", len(c["text"]), ""))
                elif c.get("type") == "thinking":
                    reqs[-1]["items"].append(("thinking", "agent's thinking", len(c.get("signature", "")) + len(c.get("thinking", "")), ""))
        elif t == "user" and reqs:
            content = e["message"]["content"]
            if isinstance(content, str):
                reqs[-1]["items"].append(("reply", "user message", len(content), content[:60]))
                continue
            for c in content:
                if c.get("type") == "tool_result":
                    src, label = names.get(c["tool_use_id"], ("?", ""))
                    body = c.get("content")
                    parts = body if isinstance(body, list) else [{"type": "text", "text": str(body)}]
                    # Text by its length; anything else but a picture (ToolSearch's tool references) by its JSON's.
                    chars = sum(len(p.get("text", "")) if p.get("type") == "text" else len(json.dumps(p)) for p in parts if p.get("type") != "image")
                    reqs[-1]["items"].append(("reply", src, chars, label))
                    for p in parts:
                        if p.get("type") == "image":
                            reqs[-1]["items"].append(("picture", src + " (pictures)", 1, label))
                elif c.get("type") == "text":
                    kind = "skill text" if c["text"].startswith("Base directory for this skill") else "user message"
                    reqs[-1]["items"].append(("reply", kind, len(c["text"]), c["text"].split("\n")[0][-60:]))
    n = len(reqs)
    # A compaction Claude Code didn't report still shows as the context shrinking by a lot.
    for k in range(1, n):
        if reqs[k]["ctx"] < reqs[k - 1]["ctx"] - 5000 and not any(c == k for c in compacted):
            compacted.append(k)
    compacted = sorted(set(c for c in compacted if 0 < c < n))
    # The requests of each stretch between compactions: request k's items reach request k + 1 only within one.
    starts = [0] + compacted
    ends = compacted + [n]
    segment_end = {k: e for b, e in zip(starts, ends) for k in range(b, e)}
    pairs = [k for k in range(n - 1) if k + 1 < segment_end[k]]
    X = np.array([[sum(s for k_, _, s, _ in reqs[k]["items"] if k_ == kind) for kind in KINDS] + [1] for k in pairs], float)
    y = np.array([reqs[k + 1]["ctx"] - reqs[k]["ctx"] for k in pairs], float)
    coef = np.linalg.lstsq(X, y, rcond=None)[0]
    r2 = 1 - ((y - X @ coef) ** 2).sum() / ((y - y.mean()) ** 2).sum()
    rate = dict(zip(KINDS, coef))
    print(f"{n} requests; context {reqs[0]['ctx']} at the first, {reqs[-1]['ctx']} at the last, {max(r['ctx'] for r in reqs)} at most; input volume {sum(r['ctx'] for r in reqs)}")
    if compacted:
        print(f"Compacted {len(compacted)} time(s), before request(s) {', '.join(str(c + 1) for c in compacted)}: the end context below is what came after the last one")
    print(f"Fit (R² {r2:.2f}): {rate['written']:.2f} tokens per character the agent wrote, {rate['reply']:.2f} per reply character, {rate['picture']:.0f} per picture, {rate['thinking']:.2f} per thinking-signature character, {coef[-1]:.0f} per request besides\n")
    print("| Message | Requests | Context at its first | at its last |")
    print("|---|---|---|---|")
    by_turn = collections.defaultdict(list)
    for r in reqs:
        by_turn[r["turn"]].append(r["ctx"])
    for t, ctxs in sorted(by_turn.items()):
        print(f"| {t} | {len(ctxs)} | {ctxs[0]} | {ctxs[-1]} |")
    tokens, count, weighted, singles = collections.Counter(), collections.Counter(), collections.Counter(), []
    vol = sum(r["ctx"] for r in reqs)
    last = starts[-1]
    for k in pairs:
        later = segment_end[k] - 1 - k  # requests that read it again
        for kind, src, size, label in reqs[k]["items"]:
            tk = rate[kind] * size
            weighted[src] += tk * later
            singles.append((tk, k + 1, src, size, label))
            if k >= last:
                tokens[src] += tk
                count[src] += 1
    end = reqs[-1]["ctx"]
    base = reqs[last]["ctx"]
    rest = end - base - sum(tokens.values())
    print(f"\n| Source | Items | Tokens in the end context | Share of it | Share of input volume |")
    print("|---|---|---|---|---|")
    first = "first request (agent's system prompt, tools)" if not compacted else "first request after the last compaction (system prompt, tools, the summary)"
    print(f"| {first} | 1 | {base} | {100 * base / end:.1f}% | {100 * sum(reqs[b]['ctx'] * (e - b) for b, e in zip(starts, ends)) / vol:.1f}% |")
    for src, tk in tokens.most_common():
        print(f"| {src} | {count[src]} | {tk:.0f} | {100 * tk / end:.1f}% | {100 * weighted[src] / vol:.1f}% |")
    print(f"| unattributed (the fit's remainder) | | {rest:.0f} | {100 * rest / end:.1f}% | |")
    print(f"\nLargest single items (estimated tokens, the request that first read it):")
    for tk, k, src, size, label in sorted(singles, reverse=True)[:top]:
        print(f"  {tk:7.0f}  req {k:3d}  {src:32s} {size:6d}  {label}")
    if "--csv" in sys.argv:
        with open(sys.argv[sys.argv.index("--csv") + 1], "w") as f:
            f.write("request,message,context\n")
            for i, r in enumerate(reqs):
                f.write(f"{i + 1},{r['turn']},{r['ctx']}\n")

main()
