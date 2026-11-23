# The MCP server

`mcp.rs`: the agent's MCP connection to a runtime. The notebook tools' own
rules are in [notebook-tools.md](notebook-tools.md); the design of the
transport, sessions and approval is in [endeavor-mcp.md](endeavor-mcp.md).

## The transport

- MCP over Streamable HTTP (spec 2025-06-18). `POST /mcp` carries one
  JSON-RPC message. A request gets its reply in the same response; a
  notification, or a response from the client, gets `202 Accepted` with no
  body.
- `initialize` echoes the client's protocol version when it is one of
  `SUPPORTED_VERSIONS`, else answers the newest. `MCP-Protocol-Version` may
  name any of them; a client that hasn't negotiated yet sends no header.
- A call that waits on the user (`Held`) answers as an event stream instead,
  which Streamable HTTP allows for a client that accepts one. Every
  `KEEP_WAITING` (15 s) it says the call is still waiting: a progress
  notification when the request asked for progress, else an SSE comment. The
  reply is its last event. This satisfies Claude Code's idle check (five
  minutes without a word) and its wait for a response to begin, and lets it
  show the wait. It doesn't lengthen Claude Code's 60 s tool timeout (unless
  `MCP_TOOL_TIMEOUT` or the server's `timeout` says otherwise), so the wait
  itself ends before that. The stream says what the call waits on (`Waits`,
  #159), and that it waits for the user only while it does.
- A held call notices when its client hangs up, by peeking at the socket.
  This isn't ported to Windows (it needs winsock's non-blocking `recv`), so
  there a call goes on after its client hangs up.
- `Mcp-Session-Id` values are visible ASCII, as the spec asks, and
  unguessable.
- JSON goes out as Julia's JSON.jl wrote it, byte for byte: object keys
  sorted by their bytes, and DEL escaped. serde_json's own key order depends
  on a feature another crate in the workspace turns on.

## Who is calling

- Each of the app's agent sessions sends `X-Endeavor-Session` (its key) and,
  on a server, `X-Endeavor-Host`; so does the stdio relay (`standalone`). A
  client with neither gets an `Mcp-Session-Id` from `initialize`, and that is
  its key: one agent connection, one session, as in the app. A client that
  sends neither header nor id is treated as the app is (no notebook of its
  own).
- `X-Endeavor-Client` (`app` or `plugin`) says who shows the user the
  notebooks: the app, or a browser. A caller that doesn't say is taken as the
  runtime's starter was, so the app and the plugin can share one runtime and
  each gets its own answers (#110).
- Someone asks the user before `approve_install` reaches the runtime only if
  the caller says so: the app (its card), or a front with
  `X-Endeavor-Consent: front` (a dialog or a page in the browser, #285,
  #389). The runtime's starter never counts, so a client `serve` set up on a
  runtime the app started isn't taken for the app. A caller no one asks for
  gets `NO_ONE_ASKS` in the tool's description and its `approve_install` is
  refused (#391): its call alone would be the yes.
- `X-Endeavor-Mode: auto`, from Claude Code's hook
  (`standalone::claude_mode`), puts a front's run in auto mode, so a run adds
  a new package without asking. Any other caller's mode is the app's to set
  (`endeavor/set_policy`).
- These headers are declarations, not checks: they stop an agent's own yes,
  since a model can't add headers to its client's requests, but not a client
  configured to send one. No text an agent reads names them.
- `X-Endeavor-Front-*` names the front's process (pid, start time, boot), so
  the runtime can tell whether the plugin is still there when the app quits
  (`Notebooks::shared`, app #232) without taking a later process with the
  same pid for it.
- `X-Endeavor-Notebooks: several` is a client that serves several
  conversations on one connection, as Claude's desktop app does.
- `X-Endeavor-Browser-Port` is the port the user's browser reaches the
  runtime on when it isn't the runtime's own (a session's loopback port).
- Header values are printable, trimmed, at most `LABEL_MAX` characters.

## Policy and asking

- The app sets each session's run policy: "plan" refuses notebook writes and
  runs; "ask" holds a call that runs code for the user's answer when the app
  says it answers runs; "auto" runs it. In Manual the app also sets `edits`,
  and then a call that changes the notebook waits for the user too.
- `WRITE_TOOLS` are the tools that change the notebook or run code, here or
  on a server. `interrupt_notebook` only stops a run, so no mode asks first,
  but plan mode refuses it.
- A large download (`approve_install`) is held in every mode but plan, Auto
  included: the user decides each one. The yes covers the packages the card
  named, not one held since (#418).
- MCP's read-only hint is what Claude Code's plan mode checks before
  prompting. `open_notebook` can run the notebook, so it isn't read-only.
- The wait for the user's answer leaves room in the 60 s agents give a call
  for the work after it: runs get at least 5 s more, and the first open in a
  new Julia takes 20 to 25 s. `run_shell` waits as a notebook run does.
- A call whose wait ended first leaves the ask up for the same call made
  again. Agents stop retrying after one to four tries whatever the text says,
  so the error also says what to do after stopping: the app keeps the card,
  and the same call when the user writes back takes their answer.
- A call that changes or runs anything takes down the session's other asks
  left up. A read doesn't: the agent may read a cell before making the same
  call again.
- An open or creation the user allowed, told its engine is still starting
  (Julia's download, say), carries on without asking again when the agent
  makes the same call: same session, same tool and arguments, nothing in
  between (another call, a Stop, a change of mode), and within
  `STARTING_CALL_KEPT` (5 min) of the yes. Counting from the yes means a start
  that stalls doesn't keep it forever; a first Julia download that takes
  longer is asked about again.
- The same `run_shell` command again waits for the one already started: the
  user was asked once.
- In Manual a denied change isn't made, even one that was to run after.
- The user's Stop in the app, or the agent cancelling a call, interrupts the
  session's runs. It is answered at once with the notebooks it looks at,
  since Pluto's interrupt can take seconds, each on its own thread.

## Results and errors

- A tool that was renamed keeps its old name for one release
  (`RENAMED_TOOLS`): `tools/list` shows only the new one, but an agent may
  have the old one from text written before (an older skill copy, an app
  from before its pin, the user's notes). Drop it the release after.
- Argument names are checked against the tool's `inputSchema`: a name the
  schema lacks, or a required one missing, fails with what the tool takes.
  Types are the tool's to check. A tool whose schema has no properties
  ignores its arguments, since some clients can't send an empty object.
- A failed call's kind is the text before the first `::` of
  `ArgumentError: kind::message`. A not-found error is a Julia `KeyError`
  that wraps its `kind::message` inside the quoted key, so the key is
  unwrapped first.
- Kinds that mean something is still starting (`STARTING_KINDS`) aren't
  marked an error: the agent waits and calls again. They keep their `error`
  code, which clients match on.
- A misuse error (`MISUSE_KINDS`) points at `notebook_guide` only for an
  agent without the plugin's skills, and only for the notebook tools' own
  mistakes, not a host tool's or a refusal that already says what to do. A
  Julia crash isn't one.
- A refusal comes first: a call that can never run isn't told to fix its
  arguments. A call with bad arguments runs nothing and isn't the session's
  activity.
- An older core refuses an R path as "must end in .jl" with no word about R
  (#226). The callers say why instead. A current core never answers an R path
  that way, since `new_notebook` hands R paths to R first.
- Without the app, results that name a notebook carry a link to it on the
  user's browser port. The link has no token: an agent is never given the
  runtime's token, which only the user's browser and terminal get. A runtime
  from before tool results dropped the token still puts it in `browser_url`,
  so it is stripped.

## Python

- The app has no pane for marimo's page yet (Endeavor #1), so the app's
  sessions are refused opening or making a Python notebook
  (`PYTHON_NOT_IN_APP`, #358). The refusal is by caller, not runtime, since
  the app and the plugin may share one. An app session is bound to its one
  notebook, so it never reaches one a plugin session opened. The app's New
  notebook never makes one.
- The notebook view has no page for Python notebooks yet (marimo's editor is
  21 MB of lazily loaded modules), so the user opens the browser link.

## Tests that guard the interface

Two tests in `mcp.rs` fail when the interface between builds changes. One
fingerprints the notebook tools' names and arguments: raise
`core::INTERFACE`, then record the new fingerprint, for an addition too,
since a newer front lists its own tools to an agent whose calls an older core
with the same number would refuse. The other lists the app's `/endeavor/call`
methods and the fields of `runtime.json`: raise `core::INTERFACE` if a caller
of the build before would get it wrong. What a call or the events stream
returns isn't caught; that stays the author's to judge.

## Asks (`asks.rs`)

In Ask to run, an agent's call that runs code waits for the user's answer;
in Manual, so does a call that changes the notebook. The app sees each
waiting call in its event stream (`asks`), shows it as a run or edit card,
and answers with `endeavor/answer_run`.

- A call waits only until its deadline, since agents end a tool call after
  about 60 s and a user can be away longer. The ask then stays up without a
  call, and the same call by the same session, on the same notebook code,
  waits on it again; an approval given meanwhile is that call's at once. The
  app sees which asks a call waits on (`waiting`), so it can keep one left up
  past the end of the agent's turn.
- Any other call by the session that changes or runs something takes it
  down, and so does a refusal of an ask no call waits on: that may be the
  app's at the end of a turn, and the next call asks afresh. Another
  session's same call is its own ask.
- A call stops waiting when the agent cancels it or its connection closes.
- Ask ids go on from the runtime's start time, so an app that saw another
  runtime's asks doesn't take a new one for an old one.
- An answer to a run carries the cells the user's own run reached while the
  call waited; an answer to `approve_install` the packages the card named
  (#418).

## Tool results (`results.rs`)

Each session keeps its last 64 tool results, for the app to look up with
`endeavor/tool_result` when the agent doesn't pass a result on (Cursor
reports only `{"success": true}`). A result is found by the id the agent's
client gave the call (Claude Code's `_meta["claudecode/toolUseId"]`, the
same id its ACP adapter gives), else as the oldest result not yet looked up
of a call to the same tool with the same arguments, in any key order and with
`1` and `1.0` the same.

## The guide (`guide.rs`)

Agents that don't load the plugin get Endeavor's skills from the
`notebook_guide` tool, and the server's MCP `instructions` say to call it
first. A session that loads the plugin says so (`X-Endeavor-Skills: plugin`),
gets no guide, and is told to load the skill instead. That pointer matters:
with only the skill's description, Haiku 4.5 starts on the tools, splits a
cell's code wrongly and skips looking at plots (#242), and with the skill but
not the reference it still defines a name in two cells.

- The guide is the notebook skill and the Julia reference; R's reference and
  the rest are topics, the skill files' links rewritten as topic names.
- How to point to a cell in a reply is in the instructions every agent gets
  on a runtime with the app, with the skills or without: `app.md` says it
  too, but agents read that for approvals and didn't link cells.
- A runtime without the app says so, since the skill keeps what holds only
  in the app apart; `endeavor mcp` adds the machine tools, and a client that
  draws the notebook view hears of it.
