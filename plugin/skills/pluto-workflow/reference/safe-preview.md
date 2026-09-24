# Safe preview (execution gated)

Notebooks opened with `open_notebook`, or created with `new_notebook`, default to **safe preview** — Pluto loads and shows cells but does not execute them yet.

**Not an edit gate:** still stage edits and call `submit_changes(wait_for_completion=false)` as usual. PlutoMCP returns an `execution_blocked` warning and keeps the cells in `pending_run` rather than dropping the work — they run once safe preview is exited.

## Exiting safe preview

There are exactly two ways out:

| Path | Who does it | When |
|------|-------------|------|
| Click "Run notebook code" in the notebook pane | **The user** | The default path — always available, no agent action needed |
| `allow_execution(notebook_id=…)` | **The agent** | Only when the user explicitly asks you to run the notebook or cells |

`allow_execution` defaults to `run_notebook=true` (queues one non-blocking full run). Pass `run_notebook=false` to exit the gate without a full run — useful when you only need your own staged cells to run: exit with `run_notebook=false`, then `submit_changes(wait_for_completion=false)` / `execute_cell` for those cells.

`run_all_cells` / `execute_cell` do **not** bypass safe preview on their own — the gate must be exited first, by one of the two paths above.

After exit, runs are non-blocking by default; poll `read_cell` / `read_notebook_code` until `!running && !queued` if you need outputs.

## Stay honest

Don't claim outputs, plots, or widgets are live until safe preview has actually been exited and the cells have run. If it's relevant to what the user asked for, say so briefly:

> This notebook is in safe preview — click "Run notebook code" above to see live output, or tell me to run it.

Only call `allow_execution` yourself when the user asks you to run something; otherwise leave the click in the pane to them.
