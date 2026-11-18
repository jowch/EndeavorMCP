# marimo adapter: spike only (2026-11-18)

Throwaway. Not wired into the core, not tested in CI. Findings and the plan
are in the project notes (`notes/marimo-spike-2026-11-18.md`).

- `adapter.py` speaks R's adapter protocol (`POST /adapter`, long-polled
  `GET /notifications?after=`), so `notebooks/r.rs` could drive it unchanged.
- `drive_spike.py` is the script that exercised it against marimo 0.25.1.

Known wrong in this prototype: `apply` and `graph` go through code mode,
which waits behind a running cell. The real adapter should edit with
`POST /api/document/transaction` + `POST /api/kernel/save` (both answer in
about 20 ms during a run; they need the page's `Marimo-Server-Token`), run
with `POST /api/kernel/run`, and build the graph with marimo's static
analysis in the adapter's own process.
