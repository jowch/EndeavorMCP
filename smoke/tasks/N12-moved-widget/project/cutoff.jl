### A Pluto.jl notebook ###
# v0.20.4

using Markdown
using InteractiveUtils

# This Pluto notebook uses @bind for interactivity. When running this notebook outside of Pluto, the following 'mock version' of @bind gives bound variables a default value (instead of an error).
macro bind(def, element)
    #! format: off
    return quote
        local iv = try Base.loaded_modules[Base.PkgId(Base.UUID("6e696c72-6542-2067-7265-42206c756150"), "AbstractPlutoDingetjes")].Bonds.initial_value catch; b -> missing; end
        local el = $(esc(element))
        global $(esc(def)) = Core.applicable(Base.get, el) ? Base.get(el) : iv(el)
        el
    end
    #! format: on
end

# ╔═╡ 5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e01
using PlutoUI

# ╔═╡ 5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e02
md"Yield cutoff (t/ha): $(@bind cutoff Slider(1.0:0.5:5.0, default=2.5, show_value=true))"

# ╔═╡ 5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e03
yields = [1.2, 2.0, 2.8, 3.1, 3.6, 4.2, 4.5, 5.0]

# ╔═╡ 5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e04
above = filter(>(cutoff), yields)

# ╔═╡ 5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e05
md"Mean yield above the cutoff: $(round(sum(above) / length(above), digits=2)) t/ha, from $(length(above)) plots"

# ╔═╡ Cell order:
# ╠═5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e01
# ╠═5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e02
# ╠═5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e03
# ╠═5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e04
# ╠═5c1d2e3f-0a4b-4c6d-8e9f-1a2b3c4d5e05
