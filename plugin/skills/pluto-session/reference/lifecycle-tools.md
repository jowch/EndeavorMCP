# Lifecycle tools

Tools relevant to session orientation and opening/creating notebooks in Endeavor. Cell-level tools (`read_cell`, `edit_cell`, `submit_changes`, …) are covered in **pluto-workflow**.

| Tool | Purpose | Notes |
|------|---------|-------|
| `pluto_session_status` | Whether Pluto is running, what notebooks are open, session info | Orientation; safe to call anytime |
| `list_notebooks` | List notebooks currently open in the session | Check before assuming a notebook isn't already open |
| `open_notebook` | Load an existing `.jl` file into the session | Requires a user-specified path; errors (`NotebookIsRunningException`) if that path is already open; defaults to safe preview |
| `new_notebook` | Create a new, empty notebook — written by Pluto itself — and load it | Pass `path` in the working directory (the default location is an internal app folder); never overwrites an existing file; opens in safe preview |
| `allow_execution` | Exit safe preview on an open notebook | Call only when the user explicitly asks you to run the notebook; `run_notebook` defaults to `true` (one non-blocking full run), pass `false` to exit the gate without running |

`open_notebook` and `new_notebook` both switch the notebook pane to that notebook automatically once they succeed — there is no separate "show it to the user" step, and no landing page to navigate.
