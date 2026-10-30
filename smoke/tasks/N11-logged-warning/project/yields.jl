### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b01
plots = [
    (plot = 1, yield = 2.5), (plot = 2, yield = missing), (plot = 3, yield = 3.4), (plot = 4, yield = 4.1),
    (plot = 5, yield = missing), (plot = 6, yield = 2.9), (plot = 7, yield = 3.8), (plot = 8, yield = 3.1),
]

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b02
cutoff = 2.0

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b03
function recorded(rows)
    kept = filter(r -> !ismissing(r.yield), rows)
    length(kept) < length(rows) && @warn "$(length(rows) - length(kept)) plots have no yield recorded and were left out"
    kept
end

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b04
kept = filter(r -> r.yield >= cutoff, recorded(plots))

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b05
md"Mean yield: $(round(sum(r.yield for r in kept) / length(kept); digits = 2)) t/ha"

# ╔═╡ Cell order:
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b01
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b02
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b03
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b04
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b05
