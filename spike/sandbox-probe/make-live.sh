#!/bin/sh
# After live.jl prints "ready": inline the live page and split it for the loader.
set -e
cd "$(dirname "$0")"
python3 -I inline.py live.html live.inner.html
python3 -I split.py live.inner.html live.parts.json
