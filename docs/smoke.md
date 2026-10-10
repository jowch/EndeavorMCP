# The agent smoke suite

The smoke suite runs a fixed list of notebook tasks through a real agent and
records whether each one passed. It checks that our stack still works with an
agent driving it: the tools, their error text, the skills, the MCP server and
the runtime. It doesn't grade models or compare agents. Claude Code is the
reference agent: the skill text is tuned until Claude passes. The plan and
the list of tasks still to write are in #26.

The real-Julia tests (`e2e_julia`, `e2e_serve`, `e2e_r`) are the half that
needs no model, and CI runs them (`.github/workflows/e2e.yml`). This page is
about the half that needs an agent.

## Running it

With Julia 1.12 and a signed-in `claude` on the PATH:

```sh
scripts/smoke.sh                       # every task in smoke/tasks
scripts/smoke.sh --only N1-new,N4-long-run
```

The script builds `endeavor` and `endeavor-smoke`, then runs the tasks one at
a time. It prints a summary and writes everything under
`target/smoke/<time>-<commit>/`. Julia is `--julia`, else `ENDEAVOR_E2E_JULIA`,
else the one `endeavor` would find. The tasks share one depot,
`target/smoke/depot` unless `--depot` names another, so packages install
once. The first run installs Pluto and takes several minutes longer.

The agent is kept apart from whoever started the run:

- It runs in a folder under the system's temporary folder, outside any
  checkout, so no `CLAUDE.md` of ours or of a parent folder reaches it.
- It gets a clean environment: the path, home, locale, proxy and certificate
  variables, and a sign-in key if one is set. A Claude Code session's own
  variables stay out, so a run started from a cloud session doesn't join that
  session or its memory.
- It uses only the project's Claude settings (`--setting-sources
  project,local`), so the user's own plugins, MCP servers and hooks stay out.
- It has only the notebook tools and `Read`, `Glob`, `Grep`, `Skill` and
  `ToolSearch` (`--tools`). `Write`, `Edit` and `Bash` don't exist for it.

The runner checks Claude's first event for each of these. If the agent had
another tool, server or plugin, or a shared memory folder, the attempt fails
as a problem with the harness, not the agent. A Claude Code cloud session can
run the suite: it has Julia, and Claude uses the sign-in in `~/.claude`.

## How a task runs

Each task is a folder in `smoke/tasks/`:

- `prompt.md`: what the user asks, in a user's words.
- `checks.json`: what decides pass or fail.
- `project/` (optional): files the project folder starts with.

For each task, the runner makes a fresh folder with its own runtime state and
a copy of `project/`. It starts `claude -p` there, with this checkout's
`claude-plugin` and `ENDEAVOR_BIN` set to `endeavor-smoke`. The plugin's
launcher then runs `endeavor-smoke mcp ...`, which is the **recording proxy**.
The proxy starts the real `endeavor mcp` with the task's state, Julia and
depot, passes stdin and stdout through, and writes every message to
`mcp.jsonl`. Because it records our own MCP traffic, the same checks work for
any agent that runs the plugin.

The proxy writes into the attempt's folder, and the plugin's own hooks still
run, as they do for a user. When the agent ends, the runner opens its own MCP session on the same
runtime and reads every open notebook: each cell's code, output and error.
Then it stops the runtime and runs the checks.

A task that fails runs twice more. It is **failing** if all three runs fail,
and **flaky** if only some do. Read why before blaming the agent: in the
first runs, N1 was flaky because its check wanted `4.978` and Claude
sometimes rounded to `4.979`. The check was wrong.

## What a run leaves

In `target/smoke/<time>-<commit>/` (each attempt runs in the temporary
folder, and is copied here without the runtime's own `state/` and `home/`):

- `summary.md` and `summary.json`: one line per task, with its status, the
  checks that failed, and the first attempt's tool calls, tool errors, time
  and cost.
- `<task>/attempt-<n>/`: `result.json` (each check and the metrics),
  `mcp.jsonl` (the proxy's log), `transcript.jsonl` (Claude's stream-json
  output), `agent-stderr.txt`, and `project/` with the notebook as the agent
  left it.

## Checks

A check is a JSON object with `"check"` naming its kind. `"soft": true`
makes it reported only: a failed soft check doesn't fail the task.

| Check | Passes when |
|---|---|
| `called` `tool`, `min` (1), `max` | the agent called `tool` between `min` and `max` times |
| `not_called` `tool` | it never called `tool` |
| `called_after_last_run` `tool` | it called `tool` after the last call that ran cells |
| `notebooks` `count` | that many notebooks are open at the end |
| `no_errored_cells` | no cell of an open notebook has an error |
| `output_contains` `texts` | some cell's output contains every one of `texts` |
| `execution_allowed` `value` | every open notebook's `execution_allowed` is `value` |
| `final_message_contains_any` `texts` | the agent's last message contains one of `texts`, ignoring case |
| `no_rerun_of_running_cells` `require_still_running` (false) | no call ran a cell that was still running, as far as the log shows: a run that returned before its cells finished, a `still_running` list, or `read_cell` and `list_notebooks` saying so; with `require_still_running`, a waited run also stopped waiting |
| `reply_contains` `texts` | some tool reply in the log contains every one of `texts`, so a number the agent reports came from the notebook |
| `all_of` / `any_of` `checks` | every one, or at least one, of the nested checks passes |
| `agent_tools_not_used` `tools` | the agent didn't use these tools of its own (read from Claude's transcript) |

An unknown check fails. A check on the last message is loose by design: it
looks for one of several phrasings. When a phrase check would decide
something subtle, make it soft.

## Tasks

| Task | Asks | Covers |
|---|---|---|
| `N1-new` | a small simulation shown as a table, in a new notebook | `new_notebook`, read, stage, run; no edits to the file |
| `N3-preview` | open a notebook from disk and say what it computes | safe preview: nothing runs, and the agent says so |
| `N4-long-run` | a cell that takes 70 s | `execution.still_running`: the cell isn't run again, and a result reported came from the notebook |
| `N9-first-package` | a DataFrame (DataFrames named) in a new notebook | a first package install: the agent waits through it and reports the table (#58). It tests the wait only on a depot without DataFrames, such as a fresh `--depot` |

## Other agents

Only part of this is Claude-specific: starting the agent (`run_claude` in
`crates/smoke/src/run.rs`), reading its transcript for the tools it used, its
last message and its cost, the isolation check on its first event, and the
`agent_tools_not_used` check. The proxy, the notebook checks and the summary
work for any agent that runs the plugin. Codex and Antigravity runs (#26)
need their own version of those parts.

## When a user reports a problem

The error code in a tool result, or what the agent did, points to a task.
For example, `stale_read` belongs to N5 once it exists, "it ran my cells
twice" is N4, and "it ran a notebook I only opened" is N3. Run that task
here at the user's version and on `main`. If it fails with Claude too, the
bug is ours. If it passes with Claude, our text reads differently to the
user's agent; tune it with their help, and keep Claude passing. A report
that no task covers becomes a new task.
