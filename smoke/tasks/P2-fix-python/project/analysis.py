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
    data = [3.0, 4.0, 5.0]
    return (data,)


@app.cell
def _(dat):
    total = sum(dat)
    return (total,)


@app.cell
def _(data, total):
    avg = total / len(data)
    return (avg,)


@app.cell
def _(avg):
    f"The average is {avg}."
    return


if __name__ == "__main__":
    app.run()
