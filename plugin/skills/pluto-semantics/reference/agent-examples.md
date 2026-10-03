# Curated agent notebook examples

Copy these layouts when authoring or editing Pluto notebooks via MCP. Each example follows [cell-structure.md](cell-structure.md).

**Do not** browse bundled Pluto `sample/` notebooks for style — many are pedagogy-oriented (micro-cells, bare `@bind` echo cells). Use **only** this doc + cell-structure.

---

## Example 1 — Interactive plot with range control

**Use when:** user wants a plot whose x-limits (or similar) are adjustable.

| # | Role | Code |
|---|------|------|
| 1 | `imports_cell` | see below |
| 2 | `widget_cell` | `@bind xrange RangeSlider(-10:0.25:10; default=-5:5)` |
| 3 | `compute_cell` | see below |

```julia
# cell 1 — imports_cell
begin
    using Plots
    using PlutoUI
end
```

```julia
# cell 2 — widget_cell
@bind xrange RangeSlider(-10:0.25:10; default=-5:5; show_value=true)
```

```julia
# cell 3 — compute_cell
begin
    lo, hi = first(xrange), last(xrange)
    plot(sinc, lo, hi, color=:red, xlabel="x", title="sinc")
end
```

---

## Example 2 — Static plot with styled chain

**Use when:** one plot needs `plot` + `plot!` + labels in one reactive step.

```julia
# imports_cell (top of notebook)
begin
    using Plots
end
```

```julia
# compute_cell — do not split plot + plot! across cells
begin
    years = 2001:2010
    values = rand(length(years)) .* 100
    plot(years, values; label="apples", legend=:topleft)
    plot!(title="Harvest", xlabel="year", ylabel="tonnes")
end
```

---

## Example 3 — Slider driving downstream value

**Use when:** one control updates a derived quantity (reactive boundary at widget).

| # | Role | Code |
|---|------|------|
| 1 | `imports_cell` | `begin; using PlutoUI; end` |
| 2 | `widget_cell` | `@bind n Slider(1:100; default=10)` |
| 3 | `compute_cell` | `begin; squares = n .^ 2; sum(squares); end` |

Do **not** add a bare `n` echo cell between widget and compute — that is pedagogy style, not agent style.

---

## Example 4 — Control embedded in markdown

**Use when:** widget should sit inline with prose.

```julia
md"""
Set level: $(@bind level Slider(1:10; default=5))
"""
```

Downstream `compute_cell` uses `level` directly.

---

## Example 5 — Button-triggered recompute (`let`)

**Use when:** user clicks to refresh expensive or random output.

```julia
# widget_cell
@bind go Button("Recompute")
```

```julia
# scoped_cell
let
    go
    md"Draw: **$(rand(1:6))**"
end
```

---

## Example 6 — Multi-control form (`combine`)

**Use when:** several inputs belong to one form layout.

```julia
begin
    using PlutoUI
end
```

```julia
@bind form PlutoUI.combine() do Child
    md"""
    Dogs: $(Child(Slider(0:10)))
    Cats: $(Child(Slider(0:10)))
    """
end
```

Downstream cells read `form` fields per PlutoUI `combine` API.

---

## Example 7 — Narrative rhythm (prose + code)

**Use when:** explaining steps to the user.

```julia
md"# Analysis"
```

```julia
begin
    using DataFrames
    using Statistics
end
```

```julia
md"Mean of `values`: **$(mean(values))**"
```

Alternate `md` cells with executable cells — no separate markdown cell type in Pluto. Add the `md` cells with `add_cell(..., folded=true)` (or `fold_cell` afterwards) so only the rendered prose shows once run.

---

## Example 8 — Fixing `pluto_multi_expression` in place

**Use when:** repairing a cell that already mixes statements.

Before (invalid):

```julia
using PlutoUI
@bind x Slider(1:10)
```

After — **split** (preferred for new structure):

- Append `using PlutoUI` to `imports_cell`
- Widget cell: `@bind x Slider(1:10)`

After — **wrap** (minimal diff on existing cell):

```julia
begin
    using PlutoUI
    @bind x Slider(1:10)
end
```

---

## Anti-patterns (do not author)

| Anti-pattern | Why |
|--------------|-----|
| `using` and `@bind` as bare adjacent lines | `pluto_multi_expression` |
| One line per statement when they share one reactive step | Noisy graph; use `compute_cell` + `begin` |
| Bare echo cell (`x` alone after `@bind x`) | Pedagogy only — merge into compute or drop (**not** Pluto `code_folded` / `fold_cell`) |
| `plot!()` mutating a plot from another cell | Stale/duplicate series — keep chain in one `begin` |
| Code after `@bind` in same cell (unwrapped) | Parse error — `@bind` must return |
| Leaving prose `md` cells unfolded | The notebook pane shows raw source instead of rendered text; use `add_cell(..., folded=true)` or `fold_cell` |

---

## Maintainer note

Examples are distilled from Pluto featured samples (`Plots.jl.jl`, `PlutoUI.jl.jl`, `Interactivity.jl`) and live agent sessions. Humans may inspect `~/.julia/packages/Pluto/*/sample/` for provenance when updating this file. **Agents: this doc only.**
