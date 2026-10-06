# Pluto reactivity

## Module scope

- All cells share one notebook module scope (`workspace#N`, bumped on reactive runs).
- Edit upstream → downstream cells re-run automatically.
- Deleting/overwriting upstream definitions removes stale globals.

## Order semantics

| Concept | Source | Effect |
|---------|--------|--------|
| **Visual order** | `cell_order` | UI layout only |
| **Execution order** | dependency graph / topological sort | What runs when |
| **Disabled cells** | `metadata["disabled"]` | Cell + downstream skipped |
| **Folded cells** | `code_folded` (`fold_cell` / `add_cell(folded=…)`) | UI presentation only — hides code, shows output; no reactive run |

Use `read_notebook_code` for execution/dependency order. Use `get_cell_order` for visual placement (`add_cell` / `move_cell`). Use `fold_cell` (or `add_cell(folded=true)`) for presentation — same metadata class as move: no `submit_changes`.

## Multiple definitions

Two cells defining the same global → `MultipleDefinitionsError` by default.

## Cycles

Non-function dependency cycles → cyclic reference error.

## MCP edit semantics

| Tool | Behavior |
|------|----------|
| `edit_cell` | Replaces entire body; default `run_after=false` |
| `submit_changes` | Batch run staged cells (Cmd+S); prefer `wait_for_completion=false` |
| `validate_cell` | Pre-check; returns `multi_expression` when invalid |

## Pluto source

- `src/evaluation/Run.jl` — `run_reactive!`, topological execution
- `src/evaluation/RunBonds.jl` — bond changes → downstream re-run
- `PlutoDependencyExplorer` — dependency graph, topological order
