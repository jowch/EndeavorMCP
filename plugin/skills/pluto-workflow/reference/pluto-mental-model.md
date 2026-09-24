# Pluto mental model (for agents)

Curated from Pluto.jl source and docs. Pluto is **not** a file you patch — it is a **reactive Julia session**.

## Core facts

- Each **cell** is one node in a dependency graph; cells share one notebook module scope.
- Pluto owns parsing, dependency analysis, execution, persistence, and pane sync.
- Notebooks save as `.jl` files, but **runtime state is in-memory** — re-read before edit.
- PlutoMCP writes **server state**; the notebook pane has its own in-place editing (last-write-wins against a concurrent edit to the same cell).
- **No hidden workspace state:** Pluto deletes/redefines globals on reactive runs (`bump_workspace_module`).
- **Visual order ≠ execution order** — use `read_notebook_code` (execution order) or dependency graph.
- **Fold = UI presentation** (`code_folded`, file markers `╟─` folded vs `╠═` open) — persisted metadata, not an execution gate; prefer `add_cell(..., folded=true)` for prose.

## What triggers re-run

Editing a cell re-runs that cell and all **downstream** dependents in topological order. Bond value changes also trigger downstream re-run.

## Agent anti-patterns (Pluto warns about these)

1. Patching `.jl` files directly — bypasses dependency analysis and pane sync
2. Assuming visual `cell_order` = execution order
3. Defining the same global in multiple cells — `MultipleDefinitionsError`
4. Expecting Jupyter-style mutable kernel state — Pluto deletes stale globals
5. Leaving safe preview on when the user needs live outputs — staged edits won't execute until the user clicks "Run notebook code" in the pane, or you call `allow_execution` because they asked

## Pluto source citations

Curated from Pluto 0.20.x. Local path: `~/.julia/packages/Pluto/*/`. Upstream: [JuliaPluto/Pluto.jl](https://github.com/JuliaPluto/Pluto.jl).

| Path | Topic |
|------|-------|
| `src/analysis/Parse.jl` | `parse_custom`, `is_single_expression`, multi-expression error |
| `src/runner/PlutoRunner/src/bonds.jl` | `@bind` macro, `Bond`, `create_bond` |
| `src/evaluation/Run.jl` | `run_reactive!`, `update_save_run!`, `will_run_code` |
| `src/evaluation/RunBonds.jl` | Bond changes → downstream re-run |
| `src/evaluation/WorkspaceManager.jl` | `bump_workspace_module`, workspace lifecycle |
| `src/notebook/Notebook.jl` | `Notebook`, `ProcessStatus`, `cell_order` |
| `src/notebook/Cell.jl` | `Cell`, `cell_id`, disabled metadata |
| `src/webserver/SessionActions.jl` | Safe preview gate on `open` |
| `src/analysis/is_just_text.jl` | Cells runnable in safe preview |

**Style:** use **pluto-semantics** `agent-examples.md` — not Pluto bundled `sample/` notebooks.
