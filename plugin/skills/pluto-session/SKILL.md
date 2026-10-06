---
name: pluto-session
description: >-
  Use when the user wants to open an existing Pluto notebook, create a new
  one, find out what notebooks are currently open, or you otherwise need
  orientation before notebook work begins and don't yet have a notebook_id.
---

# Pluto session orientation

Endeavor owns the Pluto session: one Julia process running Pluto and its notebook tools. You have no tool to start, stop, or reconnect it. If `pluto_session_status` ever reports Pluto isn't running, that's a problem to flag to the user.

## With the Endeavor app or without it

The notebook tools run in one of two settings:

- **In the Endeavor app.** The user sees the notebook in a pane next to the chat. Julia starts with the app and runs while the app is open.
- **Without the app.** The user started Julia with `endeavor serve` or `endeavor mcp` and watches the notebook on Pluto's own page in a web browser. There is no pane next to the chat.

You are working without the app when the Endeavor server's instructions say the notebooks run without the Endeavor app, or when tool results carry a `browser_url`. Otherwise you are in the app. Where this skill says "in the app" or "without the app", follow only the part for your setting.

## Where "the notebook" comes from

In the app, most turns you won't need this skill at all — prompts carry their own notebook context:

- **Viewing context.** "[Endeavor] The user is viewing Pluto notebook {id} in the notebook pane…" — that's the notebook, unless the user names a different one.
- **Annotation mode.** A preface plus `notebook://pluto/{id}/cell/{id}` links — see **pluto-workflow** → [annotations.md](../pluto-workflow/reference/annotations.md).

Reach for this skill when neither is present and you genuinely don't know which notebook is in play, or whether one exists yet. Without the app, prompts carry no such context: the notebook is the one the user names, or this session's own (see below).

## Orientation

- `pluto_session_status` — whether Pluto is running, what notebooks are open, session info.
- `list_notebooks` — what's currently open, and (`this_session`) which one is this session's.

## One notebook per session

Each agent session works on exactly one notebook. A session started from an existing notebook already has it. A session started as "New notebook" has none until you create one with `new_notebook`, and that becomes its notebook.

- `list_notebooks` also lists other sessions' notebooks. Its `this_session` field is true only for this session's notebook. Don't change a notebook whose `this_session` is false unless the user asks you to work in it. (Then open it by path, as described below.)
- If no notebook has `this_session` true and the user asks for anything that needs a notebook (code to run, a plot, an analysis), call `new_notebook` yourself right away. Don't ask the user to open or create a notebook, and don't ask them to confirm first. In the app, the notebook pane switches to the new notebook by itself.
- Once the session has its notebook, `open_notebook` and `new_notebook` refuse any other path with a `one_notebook` error, and edits or runs on another open notebook are refused the same way.
- Each notebook in `list_notebooks` has `other_sessions`: one entry for each other session working in it, with its `client` (what that session's client calls itself, or null) and `active_seconds_ago` (since its last tool call, or null if it has made none). Several sessions can work in one notebook. If another session was active in your notebook in the last few minutes, tell the user before you change it.
- You can still read any other notebook as a plain `.jl` file (for example with `Read`, or `read_file` on a server) to reuse its code or check what it did.
- If the user wants to work on a different notebook, tell them to start a new session with it. Don't try to work around the refusal.
- A notebook holds a whole line of analysis on a dataset. When the user asks for a new analysis step, add a new section to the current notebook (a markdown heading cell, then the cells for that step) instead of suggesting a new notebook.

Without the app, each agent connection is its own session: one run of `endeavor mcp`, or, over HTTP, the MCP session your client starts when it connects. The session has no notebook until you create one with `new_notebook` or open one with `open_notebook`; the first one becomes its notebook, and `list_notebooks` shows it with `this_session` true. The rules above then apply as written.

A notebook that is already open, such as one the user opened on Pluto's page in the browser or one another session opened, shows `this_session` false. If the user asks you to work in it:

- If this session has no notebook yet, call `open_notebook` with its path. It joins the notebook: it becomes this session's notebook, nothing runs, and its safe preview stays as it was. The result says `already_open`.
- If this session already has its own notebook, `open_notebook` and edits and runs in the other one are refused with `one_notebook`. Tell the user, and suggest they start a new agent session for that notebook.

## Sessions on a server

When the notebook runs on a server, the notebook and its files live on that server, not on the user's computer. In the app, your own file and shell tools (`Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`) are turned off in these sessions because they would see the Mac. Without the app, you have the tools below only if the user started `endeavor serve` with `--host-tools`. When you have them, use them for the server's files; they run on the server:

- `list_folder(path)` — what's in a folder.
- `read_file(path, offset, limit)` — read a text file (numbered lines; continue with `offset=end_line+1`).
- `run_shell(command, cwd)` — run a command, in the session's folder unless you give `cwd`. The user approves each run, so batch related steps into one command.

Paths are the server's: `~` is the server's home folder, and a relative path starts there, not in your working folder. Change notebooks only with the notebook tools, never with `run_shell` (Pluto overwrites the file). Don't use `run_shell` to sleep or to wait for cells to finish: each call asks the user again. To follow a run, poll `read_cell` until the cell is no longer running or queued.

## Idle notebooks stop

A notebook nobody has used for a while (no tool calls, edits, or running cells; the user sets how long, 48 hours by default) stops on its own. In the app, the pane then offers the user Start; without the app, the user starts it again from Pluto's start page in the browser. Only if the user asks to keep it running (say, over a long weekend), call `keep_notebook_alive(notebook_id, keep=true)`; `keep=false` undoes it. Don't call it on your own initiative.

## Opening vs. creating

| User wants | Tool |
|------------|------|
| A notebook they named (a path or a clear, unambiguous reference) | `open_notebook(path=...)` |
| A brand-new notebook | `new_notebook(path=<descriptive_name>.jl)` |
| Notebook work, and this session has no notebook yet | `new_notebook(path=<descriptive_name>.jl)` without asking |
| An existing notebook, but it's unclear which | Ask which file. Don't guess a path |

- In the app, `open_notebook` and `new_notebook` both switch the notebook pane to that notebook automatically once they succeed. There's no landing page to click through and no separate step to "show" the notebook to the user.
- Without the app, `open_notebook`, `new_notebook` and `pluto_session_status` return `browser_url`, the notebook's link on Pluto's page. Give it to the user so they can watch the notebook in their browser.
- **Pass `path` to `new_notebook` as a short descriptive `snake_case.jl` file name**, unless the user names another place. A relative path lands in the session's folder: in the app, the folder the session was started in (on a server session, that folder on the server); without the app, the folder `endeavor serve` or `endeavor mcp` was given. Without a path, Pluto picks a random name in that same folder.
- Never hand-write a `.jl` notebook file or otherwise create one outside the notebook tools (no `Write`, no generating a UUID yourself). `new_notebook` has Pluto itself write the file, and it never overwrites an existing one. Use `open_notebook` for anything that already exists on disk.
- Notebooks opened with `open_notebook` come up in **safe preview** — code is loaded but not run until the user clicks "Run notebook code" at the top of the notebook (or you call `allow_execution` because they asked). `new_notebook` skips safe preview: a new notebook has no code to distrust. See **pluto-workflow** → [safe-preview.md](../pluto-workflow/reference/safe-preview.md).

## REQUIRED chain

- Cell edits, running code, annotation comments → **pluto-workflow**
- Cell layout, `@bind`, `pluto_multi_expression` → **pluto-semantics**

## Common mistakes

| Mistake | Fix |
|---------|-----|
| `open_notebook` without a user-specified path | Never guess a path or scan the filesystem for a notebook to open |
| Ask the user to open or create a notebook when the session has none | `new_notebook` yourself |
| Edit a notebook that `list_notebooks` shows with `this_session` false | It's another session's. Work in it only if the user asks you to, by `open_notebook` on its path while this session has no notebook (see "One notebook per session"); otherwise create this session's own with `new_notebook` |
| Hand-write a new `.jl` notebook file | `new_notebook()` — let Pluto write it |
| Open or create a second notebook in the same session | Add a new section to the current notebook, read the other file with `Read`, or suggest a new session for it |
| Change a notebook that another session was just active in, without saying so | Check `other_sessions` in `list_notebooks`; if one was active in the last few minutes, tell the user before you change it |
| Ask the user to start Julia or run a setup script | Endeavor already runs Pluto and the notebook tools |

## Additional resources

- **Lifecycle tools reference:** [reference/lifecycle-tools.md](reference/lifecycle-tools.md)
- **Cell editing once a notebook is open:** [pluto-workflow](../pluto-workflow/SKILL.md)
