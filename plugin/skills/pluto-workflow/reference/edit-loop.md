# Stage-first edit loop

Pluto is a **live reactive session**, not a `.jl` file to patch. PlutoMCP writes directly to Pluto's server state; the notebook pane is a live view of that same running notebook and has its own in-place cell editing — a concurrent edit there and a `edit_cell` call on the same cell is last-write-wins.

## Pipeline

```
notebook_id from context (viewing / annotation) → read_cell (note safe preview if active)
  → read_cell / read_notebook_code
  → edit_cell / edit_cells / add_cell (run_after=false)
  → submit_changes(wait_for_completion=false)
  → if safe preview and outputs are needed: user clicks "Run notebook code" in the pane,
    or (only if they asked you to run it) allow_execution(run_notebook=false) then submit/execute the staged cells
  → read_cell (verify; poll until !running && !queued if you need outputs)
```

| Step | Tool | Notes |
|------|------|-------|
| Read | `read_cell`, `read_notebook_code` | Required before writes — PlutoMCP returns `read_required` / `stale_read` otherwise |
| Stage | `edit_cell`, `edit_cells`, `add_cell` | Default `run_after=false` |
| Fold | `fold_cell`, `add_cell(folded=…)` | Metadata only — no run / no `submit_changes`. Prefer `folded=true` for `md` prose. Verify via `read_cell` → `code_folded` |
| Validate | `validate_cell` | Optional |
| Run | `submit_changes` | Once per batch (Pluto Cmd+S); prefer `wait_for_completion=false` |
| Exit safe preview | User clicks "Run notebook code" in the pane, or `allow_execution` when they ask you to | Only needed when live outputs are required |
| Verify | `read_cell` | Check `output`, `errored`, `error`, `pending_run`; re-read if still `running`/`queued` |

## Safety

- Re-read before overwriting if the user may have been editing the cell live in the pane.
- Call **`submit_changes(wait_for_completion=false)`** before ending the turn if edits were staged.
- Don't imply safe preview was exited, or that `submit_changes` executed anything, while `execution_blocked` is still in effect.

## Cell structure

Follow **pluto-semantics** [cell-structure.md](../../pluto-semantics/reference/cell-structure.md): `imports_cell`, `widget_cell`, `compute_cell` with `begin`/`end` by default, `let`/`end` for scoped temps.

## When fixing parse errors

Wrap **`begin`/`end`** in place or split at reactive boundaries. See **pluto-semantics** [grammar.md](../../pluto-semantics/reference/grammar.md).
