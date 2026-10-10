### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b01
rate = 0.3

# ╔═╡ 7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b02
t = 0:10

# ╔═╡ 7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b03
N = 100 .* exp.(-rate .* t)

# ╔═╡ 7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b04
round(N[end]; digits = 3)

# ╔═╡ Cell order:
# ╠═7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b01
# ╠═7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b02
# ╠═7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b03
# ╠═7c3f2a51-2d4e-4f60-9bac-1d2e3f4a5b04
