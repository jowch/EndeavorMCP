### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c01
md"""
# Sensor readings

**Summary:** the mean of the cleaned readings is 4.2.
"""

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c02
md"Cleaned mean: $(round(clean_mean; digits=2)), from $(length(clean)) readings."

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c03
readings = [4.8, 5.1, 5.3, 4.9, 25.0, 5.4]

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c04
first_try = sum(readings) / length(readings)

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c05
scratch = 1 + 1

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c06
clean = filter(<(20), readings)

# ╔═╡ 3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c07
clean_mean = sum(clean) / length(clean)

# ╔═╡ Cell order:
# ╟─3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c01
# ╠═3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c02
# ╠═3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c03
# ╠═3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c04
# ╠═3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c05
# ╠═3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c06
# ╠═3c1a7e20-9b4d-4f6a-8e2c-5d0f1a2b3c07
