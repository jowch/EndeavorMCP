# Python notebooks' adapter (marimo)

`runtime/marimo/adapter.py`: Python notebooks for the runtime core
([runtime-core.md](runtime-core.md), "Python"). It answers the calls R's
adapter answers, the same way ([ember-adapter.md](ember-adapter.md)), so the
core drives it with the same upstream as R (`notebooks/r.rs`):

- `POST /adapter`: `{"method", "params"}`, answered with `{"result"}` or
  `{"error": "<Kind>: <kind>::<text>"}`, in Julia's form, which the core
  unwraps (`unwrap_key_error` in mcp.rs).
- `GET /notifications?after=<seq>`, held up to 25 s.
- Both need `Authorization: Bearer <engines' key>` (`ENDEAVOR_TOKEN`).

The core runs it with uv:

```sh
uv run --no-project --with-requirements requirements.txt python adapter.py
```

with `ENDEAVOR_TOKEN` and `ENDEAVOR_MARIMO_STATE` (where to write the ports).
The state file is written whole or not at all, since the core polls for it.
marimo's token isn't in it: the core made it.

Every call into marimo's internals is in this one file. marimo is pinned in
`requirements.in` (compiled to `requirements.txt` with
`uv pip compile requirements.in -o requirements.txt --python-version 3.12
--no-header`), with what the adapter itself uses, and tested against the pin
(`tests/e2e_marimo.rs`). The pin moves together with `scripts/cloud-setup.sh`'s
`MARIMO_VERSION` and the notebook headers in e2e_marimo and in the smoke
tasks P2, P3 and P4; `tests/marimo_pin.rs` checks them all.

## marimo's server

One `marimo edit --sandbox` server serves every notebook, so each notebook
gets its own uv environment from its PEP 723 header.

- It listens on loopback only, behind its token, which only the core's
  `/marimo/` route adds; that route forwards under the adapter's prefix. The
  core makes the token, as it does Ember's secret.
- marimo also looks for free ports for its language servers when it starts,
  and it looks on every interface, which makes Windows ask to let Python
  through the firewall. The adapter starts marimo through a small wrapper
  (`MARIMO_ON_LOOPBACK`) that makes that search use 127.0.0.1, in the
  server it starts and in the one `--sandbox` starts again in an environment
  with marimo's language-server extras. If the marimo functions it changes
  move, marimo stops with a message rather than starting unwrapped;
  `tests/e2e_marimo_windows.rs` checks both against the pinned marimo.
- Its workspace is the root of a drive: notebooks open by absolute path
  anywhere on it. On Unix that is the whole filesystem, so there is one
  marimo. On Windows the adapter starts one marimo per drive, the first at
  the root of the first Python notebook's drive (`ENDEAVOR_MARIMO_ROOT`) and
  each other when a notebook on its drive first opens. If any of them ends,
  the adapter ends, as it did with one.
- The first marimo's pages are under `/marimo/`, and the drive started n-th
  after it is under `/marimo/_n/`. `marimo.json` lists each server's root,
  base and port under `servers`. The core reads it again when a page names a
  server or a drive it hasn't seen, and it moves a page
  (`/marimo/?file=<path>`) to its drive's server with a 301. Sockets and API
  calls aren't moved: the page makes them under its own base.
- An open notebook can't move to another drive, since its server can't reach
  it there.
- Its config is ours, not the user's: nothing runs by itself (an edit marks
  its readers stale), opening runs nothing, a file edited elsewhere marks
  cells stale, and marimo's own AI is off (there is one agent). A table shows
  25 rows a page, not marimo's 10, so the agent's text of a small table is
  the whole of it.
- A new notebook is what `marimo new` writes (no cells but an empty one),
  with a PEP 723 header: its sandbox's packages. marimo is pinned to the
  server's version, or marimo adds `marimo>=<newest>` and the kernel can come
  up newer than the server. `exclude-newer` holds every package,
  dependencies included, to what was published by the time the notebook was
  made, as an R notebook's package date does. Python is held to the minor
  version it was made with, since packages from that date may have no wheels
  for a later one. Packages the notebook imports are added with their exact
  versions (`Notebook.install`).
- A notebook header that doesn't name marimo gets it, at the server's
  version, before marimo's sandbox adds `marimo>=<newest>` itself with
  `uv add --script`: that is wrong for anyone who runs the file elsewhere,
  and its kernel can come up newer than the server (#352). As marimo does, a
  file with no header gets one.

## Safe preview and going live

- In safe preview (opened from disk, not yet allowed to run) marimo has no
  session for the notebook: building the sandbox installs packages, which
  runs code. The cells come from marimo's own parser, reading the file, and an
  edit is written back with marimo's own save (its file manager, with no
  session or kernel), so nothing runs and nothing installs (#351). A change
  made on disk since is read first. The rest of a cell's config (disabled,
  column) is kept for writing an edit back.
- Once it may run, the adapter makes marimo's session as its editor (the
  sandbox builds now, then says `kernel-ready`), leaves, and watches it as a
  read-only (kiosk) connection. marimo allows one editor per notebook, so a
  page opened later takes the session over as the editor, and the page's
  token is re-keyed then, so it is asked each time. The adapter's copy of the
  cells comes from that connection, so reads answer while a cell runs.
  Writes go through the endpoints marimo's own page uses (document
  transactions, save, run, interrupt), which don't wait for a run.
- The watch is a consumer of its own, not the session's id: marimo sends an
  edit to every consumer but the one it came from, and an edit sent with the
  session's id comes from the page that holds it, which then never sees it,
  and its next save puts back what it had. The watch's own `kernel-ready` is
  taken when it connects, not while following, where it would come after
  `reform` and put back the code as the file had it.
- marimo ending the session (its kernel died, or someone shut it down there)
  is the notebook's process ending. One bad message doesn't end the watch.

### Cell ids

marimo keeps no cell ids in the file. It numbers the cells by position when it
loads it, so after a cell was added, deleted or moved (in safe preview, by
another editor, or by the agent before a restart) a cell comes back under
another cell's id (#353). So:

- Read again after a change to the file, each cell keeps the id it had.
- When the notebook goes live, the adapter gives marimo's session the ids it
  handed out before (`rekey`): safe preview's, or the last session's before a
  restart. A resumed session keeps marimo's, as its cells may have run, unless
  safe preview gave out its own (it wrote an edit, or read the file again
  after a change and kept its ids).
- The file's cells are paired with ours by code (`kept_ids`), as marimo pairs
  them when the file changes under a session: the same code first, then the
  most similar. A cell that pairs with none keeps marimo's id.
- One transaction then puts our ids back: a cell id only marimo has is
  deleted, one only we have is created, one both have takes the code, name and
  config of the cell the file holds under our id, and the order is the file's.
  Only the ids change. No id is deleted and created again, because marimo
  drops a create whose id the same transaction deleted. Nothing runs, since
  nothing has. If marimo refuses, its ids stay, the tools hear the topology
  changed, and the notebook is live either way.

### The file's form of a cell (`file_form`)

A cell's code is kept as marimo's file gives it back, when that means the same
code: a `mo.md("...")` cell is written with triple quotes, and quotes, a
trailing newline or indentation may change. Our copy and marimo's session keep
this form, not the agent's, so that they match the file. When they don't,
marimo's reload after the file changes under it (a package added to its
header) pairs cells by similar code, and can give one heading's code to
another (L1 on Haiku).

- The same meaning is compared as Python's syntax tree, but for the spaces
  and newlines around a string's text, which marimo adds to a markdown
  cell's.
- marimo writes a markdown string's value into a plain `"""`, so a backslash
  in it (LaTeX's `\alpha`) would become an escape (#433). Such a cell takes a
  raw `r"""`, which marimo keeps, unless a raw string can't hold it (it has
  `"""` or ends in a backslash). Then it is kept as sent, and since marimo's
  save writes it its own way, the cell gets a note that its code changes in
  the file (`changes_in_file`, #449). Code marimo can't write, or that
  doesn't parse, is kept as it came.
- A new session's cells come from the file as written, which may not be how
  marimo's file writes them (a hand-written `mo.md("## A")`). Its first save
  would then make the file and the session differ, and a change to the file
  under marimo (a package install) pairs them by similar code, which can give
  headings each other's code (#442, as #431 for the agent's own cells). So
  they take the file's form at once (`reform`), and the session saves, before
  anything runs. A resumed session keeps its code: its cells may have run.

## Edits

- marimo replaces a cell's whole config and refuses one missing a field, so
  the rest stays as the file had it. The adapter's copy of the config is what
  `save` writes back: a cell disabled or moved to another column on the page
  stays so after the next save (#425).
- Our copy follows an edit at once, not when marimo's message comes back: the
  next read is ours. The code each set or new cell is kept as is returned for
  the core to record as the agent's read.
- A document transaction changes the document only. A deleted cell also has
  to leave the kernel's graph, or what it defined stays there, and the cells
  that read it keep running in the session though a fresh run of the file
  fails on them (#410). marimo's `/api/kernel/delete` refuses us (no page is
  the session's editor), so it goes through code mode, which waits behind a
  running cell; the edit doesn't wait for it, and a later run queues behind
  it. Its readers are marked stale at once, as marimo will mark them in lazy
  mode: the kernel takes the cell out only after a running cell, and a read
  before that would show them current.

## Runs

- Code runs in the notebook's kernel by marimo's code mode (what
  `marimo pair` uses). It waits in the kernel's queue behind a running cell,
  so only runs and installs go this way. marimo's own run endpoint would
  refuse us: it lets only the notebook's one editor run cells, and the
  adapter is never the editor.
- marimo runs lazily here, which would leave the cells below a run stale, or
  showing the error it fixed. Pluto reruns them, and the tools say a run
  does, so the run takes them too, in the graph's order, and the core is told
  which. A run is over when its watch says so, which covers a package it
  installs and the cells it runs again.
- Cells are marked queued when the call is made, so a read straight after it
  sees the run. Cells marked queued that marimo never took (it refused the
  run) are released, and a cell another watched run still has keeps its mark
  until that run ends too.
- A cell is stale for the tools when marimo's lazy mode marked it (its inputs
  changed) or it was edited since its run. A markdown cell is any `mo.md`
  call, since marimo takes a call with keyword arguments as markdown too.
- The graph from the code (no kernel): a name defined in two cells is
  marimo's error, and those cells don't run. Run order is topological, page
  order among equals; cells in a cycle can't run. marimo's error types are
  named as Ember's errors name the same thing.
- Restart: marimo's closes the session (its page then reloads, which starts a
  new one), so the adapter starts it, with every cell not run.

## Packages

- A cell of an agent's run that fails to import a package the notebook's
  environment lacks has it added (marimo puts it in the file's header with
  its exact version) and runs again, as a Julia notebook installs a package
  on `using`. Each module is tried once. Its readers that failed only because
  it did run again with it, since marimo's lazy mode wouldn't, and they'd
  keep showing its error. The cell reads as queued until it runs again; its
  error is marked when marimo reports it, so no read sees it between the run
  and the install.
- Before the install starts (or after it ends), only the waiting cells'
  errors name it. A cell that waits because its ancestor raised names nothing
  itself: the ancestor's import error does, and that ancestor may still say
  it's running (#439).
- marimo 0.25.1 adds a sandboxed notebook's package with `uv add --script`,
  which keeps to the header's date (e2e_marimo checks it). Its other way in,
  `uv pip install`, reads no header and would take today's version, which a
  header dated before it can't build again, so the date is set for that one
  too. `ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS` (debug builds) slows it, so
  e2e_marimo's read lands during the install.
- The install under way is shown as the tools show a Julia or R one.

### Asking before a new package

A run's import of a package new to the notebook (not in its header, and not
approved yet) waits for the user's OK (#355), as a Julia notebook's does
(#397): PyPI isn't curated, so a typo such as `import pandsa` would install
whatever has that name. The front asks the user with the PyPI names
(marimo's own mapping from the import, else the module's name).

- `approve_install` is the OK: the packages install and the cells that waited
  run, under a run that `run_finished` reports the end of. Its `packages`
  are the names the user was shown, and the yes covers only those (#418); one
  added since keeps asking, and a yes that names none of what waits approves
  nothing. A yes holds for those modules while the notebook is open. Like a
  Julia notebook's reply, it says whether the install and run were started
  (`accepted`) and the notebook's `process_status` (#419). Cells that wait
  for an OK are marked as waiting for the install at once.
- Cells that wait run with the next run, and ask again for what is new then:
  an edit may have dropped the import, or the user added the package another
  way. A Stop makes them stop waiting, and they show their import error
  again. A waiting cell run from marimo's page waits no more. As a Julia run
  that asks answers, none of its cells finished.
- A name a letter or two from a widely used package's is likely a typo
  (#416), and the question and the agent hear it, though the user still
  decides. The list of import names is small and shipped in the adapter, so
  the check needs no network, and PyPI has typosquats of exactly these. A
  name on the list is never flagged. It is kept narrow, since real packages
  sit close to common ones (pysam and pyyaml, nilearn and sklearn, boto and
  boto3): the same first letter, neither name the start of the other, and one
  edit for a name under 8 letters, two from 8 up (a swap of neighbours counts
  as one); or the same name but for case, `-` or `_`.

### The package date

- `preview_packages`: what moving the header's `exclude-newer` to a date, or
  to now, would change, with nothing changed yet. uv resolves the header at
  its date, and again at the new one with each exactly pinned package let go
  to that date's newest; `changes` are the versions that differ, the
  packages they bring included. With no versions to start from, every
  package would read as added. marimo stays at the server's version. Nothing
  is built (`--no-build`), so a preview runs no package's code, in safe
  preview too. It resolves in a copy, so the notebook and its lock are
  untouched. The last ready preview is what an apply must match.
- A date is the end of that day in UTC, or now for none or a day not over
  yet, so what is published later never counts.
- `apply_packages` moves the date to the one the preview showed and pins each
  exactly pinned package at that date's version, refused unless the preview
  is ready and still what it would do. The header is written before the
  restart: marimo's save keeps the header the file has, so the session can't
  write the old one back, and a header that changed under us (a start adding
  marimo) stops it with nothing done. A live notebook then gets a new
  session, built from the new header, with every cell not run. In safe
  preview only the file changes.
- The header is written under marimo's lock for the file, as marimo's own
  `write_manifest`, but a blank line stays `#`, not `# `, so the rest of the
  user's header doesn't change. Each entry changes in place, so comments
  around it stay. A line that isn't a requirement is left as it is.

## Outputs

- HTML is interactive as the Julia runtime decides it (`_INTERACTIVE` in
  Output.jl): a script, a canvas or a control. A plotly figure comes as an
  iframe with scripts; marimo's own controls (`mo.ui.slider`,
  `mo.ui.plotly`) are `<marimo-…>` elements. A table, math or a JSON tree
  shows as text, and a layout or a static display (an accordion, tabs, a
  carousel, a sidebar, a dictionary of controls, a stat, a diagram, a
  progress bar, a menu) has its content in the HTML, so that decides. A lazy
  element's content isn't in it, so it counts. An altair chart is a
  Vega-Lite spec, which marimo draws with tooltips. A picture of such an
  output can't show that it works (#398).
- A table's text is its first page of rows, as marimo's page shows them, and
  how many rows it has in all. A list, tuple or dict is marimo's JSON, with
  its typed leaves (`text/plain+float:2.5`) as plain values. HTML's text has
  its tags dropped and entities kept simple.
