### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 7c3e1a20-5b4d-4f6a-8e21-3310a0c0d001
n = (sleep(10); 50)

# ╔═╡ 7c3e1a20-5b4d-4f6a-8e21-3310a0c0d002
total = (sleep(10); round(sum(abs2, sin.(1:n)), digits=4))

# ╔═╡ Cell order:
# ╠═7c3e1a20-5b4d-4f6a-8e21-3310a0c0d001
# ╠═7c3e1a20-5b4d-4f6a-8e21-3310a0c0d002
