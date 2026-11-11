using Pluto
const N = parse(Int, get(ENV, "NCELLS", "300"))
nbpath = joinpath(@__DIR__, "big.jl")
codes = String["md\"# Big notebook\""]
for k in 1:N÷4
    push!(codes, "md\"\"\"## Section $k\n\nSome explanation of step $k, with a few sentences of text so that the cell has a realistic height. \$(repeat(\"Lorem ipsum dolor sit amet. \", 6))\"\"\"")
    push!(codes, "x$k = [sin(i/$k) for i in 1:200]")
    push!(codes, "HTML(\"<svg width=400 height=120 viewBox='0 0 200 120'><polyline fill=none stroke=steelblue points='\" * join([\"\$i,\$(60 - 50*x$k[i])\" for i in 1:200], \" \") * \"'/></svg>\")")
    push!(codes, "HTML(\"\"\"<canvas width=200 height=40></canvas><script>const c = currentScript.parentElement.querySelector('canvas'); const g = c.getContext('2d'); g.fillStyle='orange'; g.fillRect(5,5,\$(5*$k % 190),30);</script>\"\"\")")
end
nb0 = Pluto.Notebook([Pluto.Cell(c) for c in codes], nbpath)
Pluto.save_notebook(nb0)
s = Pluto.ServerSession()
nb = Pluto.SessionActions.open(s, nbpath; run_async=false)
html = Pluto.generate_html(nb; offline_bundle=true)
write(joinpath(@__DIR__, "big.html"), html)
println("cells ", length(nb.cells), " errored ", count(c -> c.errored, nb.cells), " bytes ", length(html))
