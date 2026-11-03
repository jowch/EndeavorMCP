### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b01
rows = [split(line, ',') for line in readlines(joinpath(@__DIR__, "yields.csv"))[2:end]]

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b02
cutoff = 2.0

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b03
function parsed(rows)
    values = [(plot = parse(Int, r[1]), yield = tryparse(Float64, r[2])) for r in rows]
    kept = filter(v -> v.yield !== nothing, values)
    length(kept) < length(values) && @warn "$(length(values) - length(kept)) of $(length(values)) rows could not be read and were left out"
    kept
end

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b04
kept = filter(r -> r.yield >= cutoff, parsed(rows))

# ╔═╡ 7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b05
md"Mean yield: $(round(sum(r.yield for r in kept) / length(kept); digits = 2)) t/ha"

# ╔═╡ Cell order:
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b01
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b02
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b03
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b04
# ╠═7c3a2e50-2d4e-4f60-9b0c-1d2e3f4a5b05
