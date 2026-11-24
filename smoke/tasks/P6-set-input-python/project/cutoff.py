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
def _():
    import marimo as mo
    return (mo,)


@app.cell
def _(mo):
    cutoff = mo.ui.slider(1, 5, value=2.5, step=0.5, label="Yield cutoff (t/ha)")
    cutoff
    return (cutoff,)


@app.cell
def _():
    yields = [1.2, 2.0, 2.8, 3.1, 3.6, 4.2, 4.5, 5.0]
    return (yields,)


@app.cell
def _(cutoff, yields):
    above = [y for y in yields if y > cutoff.value]
    return (above,)


@app.cell
def _(above):
    f"Mean yield above the cutoff: {sum(above) / len(above):.2f} t/ha, from {len(above)} plots"
    return


if __name__ == "__main__":
    app.run()
