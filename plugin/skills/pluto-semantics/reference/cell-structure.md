# Notebook cell structure (agent authoring)

Pluto enforces **one expression per cell** (parse). This doc defines **roles** and **boundaries**. Copy code from [agent-examples.md](agent-examples.md).

---

## Two layers

| Layer | Rule |
|-------|------|
| **Parse** | One Julia expression per cell (`begin`/`end`, `let`/`end`, `md"…"`, `@bind`, `function`…) |
| **Structure** | Group by **reactive role**; `begin`/`end` inside a cell for multi-statement blocks |

**Split at reactive boundaries** (imports → widget → compute). Do not atomize every line when statements share one reactive step.

---

## Cell roles

| Role | Shape | Examples doc |
|------|--------|--------------|
| **imports_cell** | `begin; using …; end` — one early cell for all packages | ex. 1, 2, 6, 7 |
| **widget_cell** | `@bind var Widget(...)` or `md"…$(@bind …)…"` — one bond per input | ex. 1, 3, 4, 5, 6 |
| **compute_cell** | `begin … end` — multi-statement block that re-runs together | ex. 1, 2, 3 |
| **scoped_cell** | `let … end` — locals that must not leak to notebook scope | ex. 5 |
| **output_cell** | Single expression: `plot`, `md`, bare value | ex. 7 |
| **prose_cell** | `md"…"` added with `add_cell(folded=true)` — code hidden, rendered text shows after submit/run; `fold_cell` for existing cells | ex. 7 |

**`@bind`:** returning expression of its cell only. Bare `using` + `@bind` on adjacent lines → `pluto_multi_expression` → [agent-examples.md](agent-examples.md) ex. 8.

---

## Reactive boundaries

| Boundary | Split? |
|----------|--------|
| Imports vs rest | **Yes** |
| Each `@bind` / form | **Yes** |
| Distinct derived steps | **Often** |
| Plot chain / setup steps that must run atomically | **No** — one `compute_cell` |
| Prose vs code | **Yes** — alternate `md` and code cells |

---

## Agent vs pedagogy

Teaching notebooks use micro-cells and bare echo cells after `@bind`. **Agents do not copy that.** Use [agent-examples.md](agent-examples.md) only — not Pluto `sample/` notebooks.

---

## Quick decisions

| Situation | Action |
|-----------|--------|
| Add packages | Append to `imports_cell` |
| Add control | New `widget_cell` |
| Multi-step plot/setup | `compute_cell` with `begin`/`end` |
| Local temps | `let`/`end` |
| Add section / prose `md` | `add_cell(..., folded=true)` (or `fold_cell` after) so the notebook pane shows the rendered output only |
| `pluto_multi_expression` | [grammar.md](grammar.md) + ex. 8 |
| Dependency order unclear | `read_notebook_code` |

**Projection:** folding a cell whose code starts with `md` may omit it from default `read_notebook_code`. Use `read_cell` / `include_markdown` when you need that source.
