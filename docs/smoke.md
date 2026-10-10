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
scripts/smoke.sh --model claude-haiku-4-5    # a model other than the agent's default
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
- `followup.md` (optional): what the user asks next, in the same session,
  sent only once the agent's answer to the first has ended. An attempt where
  the agent answered fewer messages than it was sent fails.
- `checks.json`: what decides pass or fail.
- `project/` (optional): files the project folder starts with.
- `setup.json` (optional): `{ "open": ["analysis.jl"] }` opens those
  notebooks before the agent starts, allowed to run and run to the end, as if
  the user had them open already. `{ "depot": "empty" }` gives the task an
  empty depot of its own, for a first install. It is that folder alone: not
  the user's `~/.julia`, which may have the packages already.
  `{ "ssh": true }` gives the task a server (below).
- `inject.json` (optional): a second person working in the same notebook.
  It names a moment, the first time the agent calls one of `tools` (`"when":
  "before"` the call reaches the server or `"after"` its reply), and the calls
  another Endeavor session then makes, such as reading a cell and changing
  it. The proxy holds the agent's message until they are done, so the moment
  is the same in every run. The `injected` check fails if it never came.

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
Then it stops the runtime. If the task has a `reproducible` check, it copies
the task's `project/` and the notebook files the agent left into a fresh
folder, opens each notebook there in a fresh runtime, runs every cell from
the top, and reads them again. Then it runs the checks. The re-run shares the
depot, so a notebook whose packages weren't saved in its file can still pass.

Before the first task, the runner warms the shared depot: it opens
`smoke/warm/warm.jl`, which loads the packages the tasks' notebooks reach for
(DataFrames, Plots), so no task passes or fails on whether an earlier one
installed them. On a new machine this takes several minutes. N9 is the one
task with an empty depot of its own, for a first install.

A task that fails runs twice more. It is **failing** if all three runs fail,
and **flaky** if only some do. A task whose `checks.json` has
`"expected_to_fail"` naming an issue (`"#58"`) fails today on that issue: it
runs once, shows as an **expected failure**, and doesn't fail the run. When it
passes, the summary says the issue may be fixed. No task has it now. Read why before blaming the agent: in the
first runs, N1 was flaky because its check wanted `4.978` and Claude
sometimes rounded to `4.979`. The check was wrong.

## What a run leaves

In `target/smoke/<time>-<commit>/` (each attempt runs in the temporary
folder, and is copied here without the runtime's own `state/` and `home/`):

- `summary.md` and `summary.json`: one line per task, with its status, the
  checks that failed, and the first attempt's tool calls, tool errors, time
  and cost.
- `<task>/attempt-<n>/`: `result.json` (each check, the metrics, and every cell as the agent left it and as it ran again),
  `mcp.jsonl` (the proxy's log), `transcript.jsonl` (Claude's stream-json
  output), `agent-stderr.txt`, and `project/` with the notebook as the agent
  left it.

## Server tasks

A task with `{ "ssh": true }` gets a server: this computer, over real ssh.
The runner (`crates/smoke/src/ssh.rs`) starts an sshd of its own on a free
port of 127.0.0.1, with a host key and a client key made for the attempt,
and writes an ssh config that names it `smoke-host`. `gone-host` is a port
nothing listens on. OpenSSH reads `~/.ssh/config` from the account's home,
not `$HOME`, so the runtime's PATH starts with an `ssh` that runs the real
one with `-F` that config. The user's own ssh keys and config are neither
read nor changed.

The helper on the server is this `endeavor`, as for any server on the same
platform, so nothing is downloaded. Endeavor's debug-only `ENDEAVOR_TEST_ROOT`,
`_STATE` and `_DEPOT` put the server's install, its runtime's state and its
depot (the shared one) in the attempt's folder. As for the local tasks, that
depot's trailing `:` lets the server's Julia also find packages in the
account's own `~/.julia`. `{server_folder}` in a
prompt or follow-up is the attempt's folder for the server's notebooks. The
notebook checks run in a new session for the project folder, which goes to
the machine the project remembers, so after `use_machine` they read the
server's notebooks. A notebook made on this computer at the server folder's
path looks the same to them; only a check on `use_machine` tells the two apart.

It needs `sshd` and `ssh` (on Debian and Ubuntu, `openssh-server` and
`openssh-client`). sshd may also need `/run/sshd` to exist (`mkdir -p
/run/sshd`), as it does when run as root; otherwise the attempt fails as a
harness problem that says so.

On a computer with Slurm (`sinfo` on the PATH), a new server uses Slurm jobs
unless the agent passes `slurm: false`. M1's prompt says the server is the
user's own workstation with no Slurm jobs, as the tool text asks before
passing false; an agent that ignores that submits a real job there.

## Checks

A check is a JSON object with `"check"` naming its kind. `"soft": true`
makes it reported only: a failed soft check doesn't fail the task.

| Check | Passes when |
|---|---|
| `called` `tool`, `min` (1), `max`, `ok` | the agent called `tool` between `min` and `max` times; with `ok`, counting only calls that worked; with `args`, counting only calls whose arguments include those. A call turned away while Julia starts doesn't count |
| `not_called` `tool`, `args`, `before_turn` | it never called `tool`; with `before_turn` n, not before the user's n-th message, which must have been sent |
| `called_after_last_run` `tool` | it called `tool` after the last call that ran cells |
| `notebooks` `count` | that many notebooks are open at the end |
| `no_errored_cells` | no cell of an open notebook has an error |
| `output_contains` `texts` | some cell's output contains every one of `texts` |
| `execution_allowed` `value` | every open notebook's `execution_allowed` is `value` |
| `final_message_contains_any` `texts` | the agent's last message contains one of `texts`, ignoring case |
| `final_message_lacks` `texts` | the agent's last message contains none of `texts`, ignoring case |
| `server_file` `suffix` | a file ending in `suffix` is in the server's notebook folder at the end |
| `no_rerun_of_running_cells` `require_still_running` (false) | no call ran a cell that was still running, as far as the log shows: a run that returned before its cells finished, a `still_running` list, or `read_cell` and `list_notebooks` saying so; with `require_still_running`, a waited run also stopped waiting |
| `reply_contains` `texts` | some tool reply in the log contains every one of `texts`, so a number the agent reports came from the notebook |
| `all_of` / `any_of` `checks` | every one, or at least one, of the nested checks passes |
| `injected` | the second person in `inject.json` made all its calls |
| `code_contains` `texts`, `not` | some cell's code contains every one of `texts` and none of `not` |
| `ran_after_reply` `texts` | after a tool reply containing every one of `texts` (an error to deal with), a later call ran cells and worked |
| `reproducible` | run again from its file in a fresh runtime and a fresh copy of the project folder, every notebook gives the same output and errors, cell by cell. Cells still running when the agent ended, and pictures, aren't compared, and a notebook with nothing left to compare fails |
| `agent_tools_not_used` `tools` | the agent didn't use these tools of its own (read from Claude's transcript) |

An unknown check fails. A check on the last message is loose by design: it
looks for one of several phrasings. When a phrase check would decide
something subtle, make it soft.

## Tasks

| Task | Asks | Covers |
|---|---|---|
| `N1-new` | a small simulation shown as a table, in a new notebook | `new_notebook`, read, stage, run; no edits to the file |
| `N2-fix` | fix the error in a notebook that is already open | joining an open notebook (`already_open`), errors, the cells that depend on the fix |
| `N3-preview` | open a notebook from disk and say what it computes | safe preview: nothing runs, and the agent says so |
| `N4-long-run` | a cell that takes 70 s | `execution.still_running`: the cell isn't run again, and a result reported came from the notebook |
| `N5-stale` | change a value; someone else changes it right after the agent reads it | `stale_read`: the agent reads again and tells the user, not overwriting silently |
| `N6-conflict` | change a value; someone else changes a cell it depends on just before the edit | `run_conflict`: the agent reads the change, runs again, keeps the other person's edit |
| `N7-plot` | a plot | the agent looks at the picture (`view_cell_output`) before it reports |
| `N8-one-notebook` | N1, then "make a separate notebook" in the same session | `one_notebook`: no second notebook; a section in this one, or a new session |
| `N9-cold-install` | a DataFrame in a new notebook, on an empty depot | a first install: the agent waits through it and reports the table (#58) |
| `M1-machine` | add my server and compute something in a notebook there; then "yes, install it" | `add_machine`'s `needs_install`: the agent asks first and installs only after the yes, then works on the server |
| `M2-no-reach` | a notebook on a server that refuses connections | the agent reports the failure, doesn't ask for a password, and makes no notebook here instead |

Every task that leaves a notebook also checks it is `reproducible`, except
N3, whose notebook isn't meant to run, N9, and the server tasks, whose
notebooks the re-run can't reach.

Each `result.json` and the summary record the model or models that answered
(from Claude's own events), so runs can be compared across models later.

## Other agents

Only part of this is Claude-specific: starting the agent (`run_claude` in
`crates/smoke/src/run.rs`), reading its transcript for the tools it used, its
last message and its cost, the isolation check on its first event, and the
`agent_tools_not_used` check. The proxy, the notebook checks and the summary
work for any agent that runs the plugin. Codex and Antigravity runs (#26)
need their own version of those parts.

## When a user reports a problem

Users report a problem with the agent on the issue form "The agent did
something wrong" (`.github/ISSUE_TEMPLATE/agent-problem.yml`). It asks for
the agent and its version, the model if known, Endeavor's version, where the
notebook ran, the prompt, what went wrong, any error code in the tool
results, the agent's last message and `endeavor status`, and tells people
to take out secrets, folder paths and server names first.

1. **Find the task.** The error code, or what the agent did, points to one:

   | In the report | Task |
   |---|---|
   | `already_open`, "it couldn't fix the error in my open notebook" | N2 |
   | `stale_read` | N5 |
   | `run_conflict` | N6 |
   | `one_notebook`, a second notebook | N8 |
   | `still_running`, "it ran my cells twice" | N4 |
   | "it ran a notebook I only opened" | N3 |
   | a plot it described without looking | N7 |
   | `needs_install`, an install it didn't ask about | M1 |
   | a server it couldn't reach, a password prompt | M2 |
   | a first package install that looked stuck | N9 |

2. **Find whose problem it is.** Run that task with Claude at the reporter's
   build, then on `main`. A `Release:` line in their `endeavor status` means
   a released build: `scripts/helpers.sh --key` at a commit prints that key,
   so finding the commit needs no build. A local build has no such line, and
   its build in `endeavor --version` is a hash of the source, not a commit:
   finding it means building commits until one matches. Often `main` alone
   settles it. If it fails with Claude too, the bug is ours: fix it, and the task
   now guards it. If it passes with Claude and the reporter used another
   agent, that agent reads our text differently: tune the skill text with the
   reporter's help, and keep Claude passing. If it passes everywhere, it
   depended on their data, model or setup; ask for the notebook.
3. **No task covers it:** write one from the report, with a prompt as close
   to theirs as their data allows.
