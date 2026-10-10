### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 3a1f0c52-8a7e-4b61-9d3c-2f6e1b0a7c01
weights = [2.0, 4.0, 9.0, 3.0]

# ╔═╡ 3a1f0c52-8a7e-4b61-9d3c-2f6e1b0a7c02
g = exp(sum(log.(weights)) / length(weights))

# ╔═╡ 3a1f0c52-8a7e-4b61-9d3c-2f6e1b0a7c03
ratio = g / (sum(weights) / length(weights))

# ╔═╡ Cell order:
# ╠═3a1f0c52-8a7e-4b61-9d3c-2f6e1b0a7c01
# ╠═3a1f0c52-8a7e-4b61-9d3c-2f6e1b0a7c02
# ╠═3a1f0c52-8a7e-4b61-9d3c-2f6e1b0a7c03
