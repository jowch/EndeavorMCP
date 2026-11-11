using Pluto
nbpath = "/tmp/claude-0/spike/nb/demo.jl"
codes = [
 "md\"# Demo notebook\"",
 "@bind n html\"<input type=range min=1 max=20 value=5>\"",
 "squares = [i^2 for i in 1:something(n, 5)]",
 "m = 7",
 """HTML(\"\"\"<canvas width=200 height=60 style="border:1px solid #888"></canvas>
<script>
const c = currentScript.parentElement.querySelector("canvas");
const g = c.getContext("2d"); g.fillStyle = "orange"; g.fillRect(10,10,\$(10*m),40);
</script>\"\"\")""",
]
nb0 = Pluto.Notebook([Pluto.Cell(c) for c in codes], nbpath)
Pluto.save_notebook(nb0)
s = Pluto.ServerSession()
nb = Pluto.SessionActions.open(s, nbpath; run_async=false)
html = Pluto.generate_html(nb; offline_bundle=true)
write("/tmp/claude-0/spike/nb/static.html", html)
println("bytes ", length(html))
for c in nb.cells; println(c.cell_id, " errored=", c.errored, " ", first(string(c.output.body), 80)); end
