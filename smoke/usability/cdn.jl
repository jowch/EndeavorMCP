### A Pluto.jl notebook ###
# v0.20.0

using Markdown
using InteractiveUtils

macro bind(def, element)
    quote
        local iv = try Base.loaded_modules[Base.PkgId(Base.UUID("6e696c72-6542-2067-7265-42206c756150"), "AbstractPlutoDingetjes")].Bonds.initial_value catch; b -> missing; end
        local el = $(esc(element))
        global $(esc(def)) = Core.applicable(Base.get, el) ? Base.get(el) : iv(el)
        el
    end
end

# ╔═╡ 9e6a1b70-dda7-11f1-a148-000000000001
md"# CDN check

Each line below says `function` when the CDN script loaded, and `undefined` or `failed` when the view blocked it."

# ╔═╡ 9e6a1b70-dda7-11f1-a148-000000000002
html"""<p>Script tag: <b id=cdnprobe>pending</b></p><script src="https://cdn.jsdelivr.net/npm/canvas-confetti@1.9.3/dist/confetti.browser.min.js"></script><script>document.getElementById("cdnprobe").textContent = typeof confetti</script>"""

# ╔═╡ 9e6a1b70-dda7-11f1-a148-000000000003
html"""<p>Module import: <b id=esmprobe>pending</b></p><script>try { const m = await import("https://esm.sh/canvas-confetti@1.9.3"); document.getElementById("esmprobe").textContent = typeof m.default } catch (e) { document.getElementById("esmprobe").textContent = "failed: " + e.message }</script>"""

# ╔═╡ 9e6a1b70-dda7-11f1-a148-000000000004
html"""<p>Fetch: <b id=fetchprobe>pending</b></p><script>fetch("https://cdn.jsdelivr.net/npm/canvas-confetti@1.9.3/package.json").then(r => r.json()).then(j => document.getElementById("fetchprobe").textContent = "function (" + j.name + ")", e => document.getElementById("fetchprobe").textContent = "failed: " + e.message)</script>"""

# ╔═╡ 9e6a1b70-dda7-11f1-a148-000000000005
html"""<p>Code font in use: <b id=fontprobe>pending</b></p><script>setTimeout(async () => { await document.fonts.ready; const loaded = [...document.fonts].filter(f => f.status === "loaded").map(f => f.family); document.getElementById("fontprobe").textContent = (loaded.length ? [...new Set(loaded)].join(", ") : "none loaded, system monospace") }, 3000)</script>"""

# ╔═╡ Cell order:
# ╠═9e6a1b70-dda7-11f1-a148-000000000001
# ╠═9e6a1b70-dda7-11f1-a148-000000000002
# ╠═9e6a1b70-dda7-11f1-a148-000000000003
# ╠═9e6a1b70-dda7-11f1-a148-000000000004
# ╠═9e6a1b70-dda7-11f1-a148-000000000005
