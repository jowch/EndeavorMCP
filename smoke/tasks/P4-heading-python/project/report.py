# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "marimo==0.25.1",
# ]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///

import marimo

__generated_with = "0.25.1"
app = marimo.App()


@app.cell
def _(mo):
    mo.md("## Setup")
    return


@app.cell
def _():
    import marimo as mo
    return (mo,)


@app.cell
def _(mo):
    mo.md("## Data")
    return


@app.cell
def _():
    counts = [12, 7, 9, 15]
    return (counts,)


@app.cell
def _(mo):
    mo.md("## Data Checks")
    return


@app.cell
def _(counts):
    f"All counts positive: {min(counts) > 0}"
    return


@app.cell
def _(mo):
    mo.md("## Results")
    return


@app.cell
def _(counts):
    f"The total is {sum(counts)}."
    return


if __name__ == "__main__":
    app.run()
