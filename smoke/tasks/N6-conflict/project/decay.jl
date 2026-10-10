### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 8d4a3b62-3e5f-4071-acbd-2e3f4a5b6c01
t = 0:10

# ╔═╡ 8d4a3b62-3e5f-4071-acbd-2e3f4a5b6c02
N = 100 .* exp.(-0.3 .* t)

# ╔═╡ 8d4a3b62-3e5f-4071-acbd-2e3f4a5b6c03
round(N[end]; digits = 3)

# ╔═╡ Cell order:
# ╠═8d4a3b62-3e5f-4071-acbd-2e3f4a5b6c01
# ╠═8d4a3b62-3e5f-4071-acbd-2e3f4a5b6c02
# ╠═8d4a3b62-3e5f-4071-acbd-2e3f4a5b6c03
