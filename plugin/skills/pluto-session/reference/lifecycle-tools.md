# Lifecycle tools

Tools relevant to session orientation and opening/creating notebooks in Endeavor. Cell-level tools (`read_cell`, `edit_cell`, `submit_changes`, …) are covered in **pluto-workflow**.

| Tool | Purpose | Notes |
|------|---------|-------|
| `pluto_session_status` | Whether Pluto is running, what notebooks are open, session info | Orientation; safe to call anytime |
| `list_notebooks` | List notebooks currently open, including other sessions' | `this_session` marks this session's own notebook; if none has it, create one with `new_notebook` when the user asks for notebook work |
| `open_notebook` | Load an existing `.jl` file into the session | Requires a user-specified path; errors (`notebook_already_open`, with the open notebook's id) if that path is already open; defaults to safe preview |
| `new_notebook` | Create a new, empty notebook — written by Pluto itself — and load it | Pass a descriptive file name as `path` (relative paths and the default land in the session's folder); never overwrites an existing file; ready to run (no safe preview) |
| `allow_execution` | Exit safe preview on an open notebook | Call only when the user explicitly asks you to run the notebook; `run_notebook` defaults to `true` (one non-blocking full run), pass `false` to exit the gate without running |
| `keep_notebook_alive` | Exempt an open notebook from the idle stop (`keep=true`), or undo that (`keep=false`) | Call only when the user asks to keep the notebook running; lasts until turned off or the notebook is stopped |

Each session works on one notebook. After the session has it, `open_notebook` and `new_notebook` refuse other paths with a `one_notebook` error; read other notebooks as plain files instead.

`open_notebook` and `new_notebook` both switch the notebook pane to that notebook automatically once they succeed — there is no separate "show it to the user" step, and no landing page to navigate.
