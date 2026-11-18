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
    import math
    weights = [2.0, 4.0, 9.0, 3.0]
    return math, weights


@app.cell
def _(math, weights):
    g = math.exp(sum(math.log(w) for w in weights) / len(weights))
    return (g,)


@app.cell
def _(g, weights):
    ratio = g / (sum(weights) / len(weights))
    return


if __name__ == "__main__":
    app.run()
