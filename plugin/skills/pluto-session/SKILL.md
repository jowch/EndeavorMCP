---
name: pluto-session
description: >-
  Use when the user wants to open an existing Pluto notebook, create a new
  one, find out what notebooks are currently open, or you otherwise need
  orientation before notebook work begins and don't yet have a notebook_id.
---

# Pluto session orientation

Endeavor owns the Pluto session: one Julia process running Pluto + PlutoMCP starts with the app and stays running the whole time the app is open. You never start it, stop it, reconnect it, or set it up — `start_pluto_session` / `stop_pluto_session` are not part of normal use. If `pluto_session_status` ever reports Pluto isn't running, that's an app-level problem to flag to the user, not something to fix by calling lifecycle tools yourself.

## Where "the notebook" comes from

Most turns you won't need this skill at all — prompts carry their own notebook context:

- **Viewing context.** "[Endeavor] The user is viewing Pluto notebook {id} in the notebook pane…" — that's the notebook, unless the user names a different one.
- **Annotation mode.** A preface plus `pluto://notebook/{id}/cell/{id}` links — see **pluto-workflow** → [annotations.md](../pluto-workflow/reference/annotations.md).

Reach for this skill when neither is present and you genuinely don't know which notebook is in play, or whether one exists yet.

## Orientation

- `pluto_session_status` — whether Pluto is running, what notebooks are open, session info.
- `list_notebooks` — what's currently open in the session.

## Opening vs. creating

| User wants | Tool |
|------------|------|
| A notebook they named (a path or a clear, unambiguous reference) | `open_notebook(path=...)` |
| A brand-new notebook | `new_notebook(path=<working directory>/<descriptive_name>.jl)` |
| Unclear, or no notebook named | `list_notebooks`, then ask — or offer to create one with `new_notebook` |

- `open_notebook` and `new_notebook` both switch the notebook pane to that notebook automatically once they succeed. There's no landing page to click through and no separate step to "show" the notebook to the user.
- **Always pass `path` to `new_notebook`, inside your current working directory** (the folder this Endeavor session was started in), with a short descriptive `snake_case.jl` name, unless the user names another place. Without a path, Pluto puts the file in an internal app folder the user won't find.
- Never hand-write a `.jl` notebook file or otherwise create one outside PlutoMCP (no `Write`, no generating a UUID yourself). `new_notebook` has Pluto itself write the file, and it never overwrites an existing one. Use `open_notebook` for anything that already exists on disk.
- Freshly opened or newly created notebooks come up in **safe preview** — code is loaded but not run until the user clicks "Run notebook code" in the pane (or you call `allow_execution` because they asked). See **pluto-workflow** → [safe-preview.md](../pluto-workflow/reference/safe-preview.md).

## REQUIRED chain

- Cell edits, running code, annotation comments → **pluto-workflow**
- Cell layout, `@bind`, `pluto_multi_expression` → **pluto-semantics**

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Call `start_pluto_session` / `stop_pluto_session` | Not needed — Endeavor manages the Pluto process for the whole app session |
| `open_notebook` without a user-specified path | Never guess a path or scan the filesystem for a notebook to open |
| Hand-write a new `.jl` notebook file | `new_notebook()` — let Pluto write it |
| Re-`open_notebook` a path that's already open | `list_notebooks` first; if it's there, use its `notebook_id` (re-opening errors) |
| Ask the user to start Julia or run a setup script | Endeavor already runs Pluto + PlutoMCP |

## Additional resources

- **Lifecycle tools reference:** [reference/lifecycle-tools.md](reference/lifecycle-tools.md)
- **Cell editing once a notebook is open:** [pluto-workflow](../pluto-workflow/SKILL.md)
