### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 7c3a0000-2b1d-4e6f-9a8b-1c2d3e4f5a00
md"# Plate reader analysis
Absorbance readings for 24 wells, normalized against the blank, then filtered by a threshold."

# ╔═╡ 7c3a0001-2b1d-4e6f-9a8b-1c2d3e4f5a01
md"## Raw readings"

# ╔═╡ 7c3a0002-2b1d-4e6f-9a8b-1c2d3e4f5a02
n_wells = 24

# ╔═╡ 7c3a0003-2b1d-4e6f-9a8b-1c2d3e4f5a03
raw = let
	x = 12345
	vals = Float64[]
	for i in 1:n_wells
		x = (1103515245 * x + 12345) % 2^31
		push!(vals, round(0.1 + 1.9 * x / 2^31; digits = 3))
	end
	vals
end

# ╔═╡ 7c3a0004-2b1d-4e6f-9a8b-1c2d3e4f5a04
blank = 0.08

# ╔═╡ 7c3a0005-2b1d-4e6f-9a8b-1c2d3e4f5a05
wells = ["A$(i)" for i in 1:n_wells]

# ╔═╡ 7c3a0006-2b1d-4e6f-9a8b-1c2d3e4f5a06
md"## Normalization"

# ╔═╡ 7c3a0007-2b1d-4e6f-9a8b-1c2d3e4f5a07
corrected = raw .- blank

# ╔═╡ 7c3a0008-2b1d-4e6f-9a8b-1c2d3e4f5a08
top = maximum(corrected)

# ╔═╡ 7c3a0009-2b1d-4e6f-9a8b-1c2d3e4f5a09
normalized = round.(corrected ./ top; digits = 3)

# ╔═╡ 7c3a000a-2b1d-4e6f-9a8b-1c2d3e4f5a0a
md"Each reading is divided by the largest one, so the brightest well is 1.0."

# ╔═╡ 7c3a000b-2b1d-4e6f-9a8b-1c2d3e4f5a0b
md"## Summary statistics"

# ╔═╡ 7c3a000c-2b1d-4e6f-9a8b-1c2d3e4f5a0c
mean_norm = round(sum(normalized) / n_wells; digits = 3)

# ╔═╡ 7c3a000d-2b1d-4e6f-9a8b-1c2d3e4f5a0d
sorted_norm = sort(normalized)

# ╔═╡ 7c3a000e-2b1d-4e6f-9a8b-1c2d3e4f5a0e
median_norm = (sorted_norm[12] + sorted_norm[13]) / 2

# ╔═╡ 7c3a000f-2b1d-4e6f-9a8b-1c2d3e4f5a0f
spread = round(maximum(normalized) - minimum(normalized); digits = 3)

# ╔═╡ 7c3a0010-2b1d-4e6f-9a8b-1c2d3e4f5a10
md"Mean $(mean_norm), median $(median_norm), spread $(spread)."

# ╔═╡ 7c3a0011-2b1d-4e6f-9a8b-1c2d3e4f5a11
md"## Rows of the plate"

# ╔═╡ 7c3a0012-2b1d-4e6f-9a8b-1c2d3e4f5a12
row_A = normalized[1:6]

# ╔═╡ 7c3a0013-2b1d-4e6f-9a8b-1c2d3e4f5a13
row_A_mean = round(sum(row_A) / 6; digits = 3)

# ╔═╡ 7c3a0014-2b1d-4e6f-9a8b-1c2d3e4f5a14
row_B = normalized[7:12]

# ╔═╡ 7c3a0015-2b1d-4e6f-9a8b-1c2d3e4f5a15
row_B_mean = round(sum(row_B) / 6; digits = 3)

# ╔═╡ 7c3a0016-2b1d-4e6f-9a8b-1c2d3e4f5a16
row_C = normalized[13:18]

# ╔═╡ 7c3a0017-2b1d-4e6f-9a8b-1c2d3e4f5a17
row_C_mean = round(sum(row_C) / 6; digits = 3)

# ╔═╡ 7c3a0018-2b1d-4e6f-9a8b-1c2d3e4f5a18
row_D = normalized[19:24]

# ╔═╡ 7c3a0019-2b1d-4e6f-9a8b-1c2d3e4f5a19
row_D_mean = round(sum(row_D) / 6; digits = 3)

# ╔═╡ 7c3a001a-2b1d-4e6f-9a8b-1c2d3e4f5a1a
row_means = [row_A_mean, row_B_mean, row_C_mean, row_D_mean]

# ╔═╡ 7c3a001b-2b1d-4e6f-9a8b-1c2d3e4f5a1b
md"Row means: $(row_means)."

# ╔═╡ 7c3a001c-2b1d-4e6f-9a8b-1c2d3e4f5a1c
md"## Threshold"

# ╔═╡ 7c3a001d-2b1d-4e6f-9a8b-1c2d3e4f5a1d
threshold = 0.5

# ╔═╡ 7c3a001e-2b1d-4e6f-9a8b-1c2d3e4f5a1e
passing = [w for (w, v) in zip(wells, normalized) if v >= threshold]

# ╔═╡ 7c3a001f-2b1d-4e6f-9a8b-1c2d3e4f5a1f
n_passing = length(passing)

# ╔═╡ 7c3a0020-2b1d-4e6f-9a8b-1c2d3e4f5a20
fraction_passing = round(n_passing / n_wells; digits = 3)

# ╔═╡ 7c3a0021-2b1d-4e6f-9a8b-1c2d3e4f5a21
md"**$(n_passing) of $(n_wells) wells** are at or above the threshold of $(threshold)."

# ╔═╡ 7c3a0022-2b1d-4e6f-9a8b-1c2d3e4f5a22
md"## Wells below the threshold"

# ╔═╡ 7c3a0023-2b1d-4e6f-9a8b-1c2d3e4f5a23
failing = setdiff(wells, passing)

# ╔═╡ 7c3a0024-2b1d-4e6f-9a8b-1c2d3e4f5a24
failing_values = [normalized[findfirst(==(w), wells)] for w in failing]

# ╔═╡ 7c3a0025-2b1d-4e6f-9a8b-1c2d3e4f5a25
md"## Notes"

# ╔═╡ 7c3a0026-2b1d-4e6f-9a8b-1c2d3e4f5a26
md"The blank was measured once at the start of the run."

# ╔═╡ 7c3a0027-2b1d-4e6f-9a8b-1c2d3e4f5a27
md"Readings above 1.9 would saturate the reader; none here do."

# ╔═╡ 7c3a0028-2b1d-4e6f-9a8b-1c2d3e4f5a28
saturated = count(>(1.9), raw)

# ╔═╡ 7c3a0029-2b1d-4e6f-9a8b-1c2d3e4f5a29
md"Saturated wells: $(saturated)."

# ╔═╡ Cell order:
# ╠═7c3a0000-2b1d-4e6f-9a8b-1c2d3e4f5a00
# ╠═7c3a0001-2b1d-4e6f-9a8b-1c2d3e4f5a01
# ╠═7c3a0002-2b1d-4e6f-9a8b-1c2d3e4f5a02
# ╠═7c3a0003-2b1d-4e6f-9a8b-1c2d3e4f5a03
# ╠═7c3a0004-2b1d-4e6f-9a8b-1c2d3e4f5a04
# ╠═7c3a0005-2b1d-4e6f-9a8b-1c2d3e4f5a05
# ╠═7c3a0006-2b1d-4e6f-9a8b-1c2d3e4f5a06
# ╠═7c3a0007-2b1d-4e6f-9a8b-1c2d3e4f5a07
# ╠═7c3a0008-2b1d-4e6f-9a8b-1c2d3e4f5a08
# ╠═7c3a0009-2b1d-4e6f-9a8b-1c2d3e4f5a09
# ╠═7c3a000a-2b1d-4e6f-9a8b-1c2d3e4f5a0a
# ╠═7c3a000b-2b1d-4e6f-9a8b-1c2d3e4f5a0b
# ╠═7c3a000c-2b1d-4e6f-9a8b-1c2d3e4f5a0c
# ╠═7c3a000d-2b1d-4e6f-9a8b-1c2d3e4f5a0d
# ╠═7c3a000e-2b1d-4e6f-9a8b-1c2d3e4f5a0e
# ╠═7c3a000f-2b1d-4e6f-9a8b-1c2d3e4f5a0f
# ╠═7c3a0010-2b1d-4e6f-9a8b-1c2d3e4f5a10
# ╠═7c3a0011-2b1d-4e6f-9a8b-1c2d3e4f5a11
# ╠═7c3a0012-2b1d-4e6f-9a8b-1c2d3e4f5a12
# ╠═7c3a0013-2b1d-4e6f-9a8b-1c2d3e4f5a13
# ╠═7c3a0014-2b1d-4e6f-9a8b-1c2d3e4f5a14
# ╠═7c3a0015-2b1d-4e6f-9a8b-1c2d3e4f5a15
# ╠═7c3a0016-2b1d-4e6f-9a8b-1c2d3e4f5a16
# ╠═7c3a0017-2b1d-4e6f-9a8b-1c2d3e4f5a17
# ╠═7c3a0018-2b1d-4e6f-9a8b-1c2d3e4f5a18
# ╠═7c3a0019-2b1d-4e6f-9a8b-1c2d3e4f5a19
# ╠═7c3a001a-2b1d-4e6f-9a8b-1c2d3e4f5a1a
# ╠═7c3a001b-2b1d-4e6f-9a8b-1c2d3e4f5a1b
# ╠═7c3a001c-2b1d-4e6f-9a8b-1c2d3e4f5a1c
# ╠═7c3a001d-2b1d-4e6f-9a8b-1c2d3e4f5a1d
# ╠═7c3a001e-2b1d-4e6f-9a8b-1c2d3e4f5a1e
# ╠═7c3a001f-2b1d-4e6f-9a8b-1c2d3e4f5a1f
# ╠═7c3a0020-2b1d-4e6f-9a8b-1c2d3e4f5a20
# ╠═7c3a0021-2b1d-4e6f-9a8b-1c2d3e4f5a21
# ╠═7c3a0022-2b1d-4e6f-9a8b-1c2d3e4f5a22
# ╠═7c3a0023-2b1d-4e6f-9a8b-1c2d3e4f5a23
# ╠═7c3a0024-2b1d-4e6f-9a8b-1c2d3e4f5a24
# ╠═7c3a0025-2b1d-4e6f-9a8b-1c2d3e4f5a25
# ╠═7c3a0026-2b1d-4e6f-9a8b-1c2d3e4f5a26
# ╠═7c3a0027-2b1d-4e6f-9a8b-1c2d3e4f5a27
# ╠═7c3a0028-2b1d-4e6f-9a8b-1c2d3e4f5a28
# ╠═7c3a0029-2b1d-4e6f-9a8b-1c2d3e4f5a29
