# Cell grammar (Pluto authoritative)

Enforced in Pluto `parse_custom` (`src/analysis/Parse.jl`):

- Each code cell = **exactly one Julia expression**
- Multiple bare statements → `extra token after end of expression` + `Boundaries: [...]`
- MCP maps this to `error.kind = pluto_multi_expression`

**Structure model (agent authoring):** [cell-structure.md](cell-structure.md)

## Valid single-expression forms

| Form | Valid? |
|------|--------|
| `begin … end` / `let … end` | Yes — one compound expression |
| `a; b` (semicolon chain) | Yes — last value is cell output |
| `function f() … end` | Yes |
| `md"…"` / `html"""…"""` | Yes |
| `@bind` / `combine() do … end` | Yes — when it is the cell result |
| Two statements on separate lines | **No** |

## Authoring new cells

| Role | Guidance |
|------|----------|
| `using` / `import` | **`imports_cell`** — one early `begin`/`end` block for all packages |
| Multi-statement work | **`compute_cell`** — default `begin`/`end` for one conceptual block |
| Local temporaries | **`scoped_cell`** — `let`/`end` so names do not leak to notebook scope |
| `@bind` | **`widget_cell`** — returning expression only; see cell-structure |
| Prose | `md"…"` Julia cell; alternate with code cells |

Do **not** atomize every line into its own cell when statements share one reactive step. **Do** split at widget and import boundaries.

## Fixing `pluto_multi_expression`

Two valid fixes — see [agent-examples.md](agent-examples.md) ex. 8:

**Wrap** — minimal change, same cell:

```julia
begin
    using Plots
    plot(sin, 0, 2pi)
end
```

**Split** — when separate reactive steps are intended:

| Cell | Code |
|------|------|
| 1 | `begin; using Plots; end` (or append to `imports_cell`) |
| 2 | `plot(sin, 0, 2pi)` |

Default to **wrap** when repairing an existing cell; default to **split** when the statements are genuinely independent reactive inputs.

## `@bind` rule (from Pluto source)

`@bind` expands to `create_bond(...)` which returns a `Bond` — that **is** the cell output. Bare code after `@bind` → parse error. Consumers live in downstream cells unless wrapped in `begin`/`end` with `@bind` last.

## Trailing semicolon

`ends_with_semicolon` suppresses output display.
