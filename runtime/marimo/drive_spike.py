import asyncio, json, sys, time, httpx, websockets, base64
B="http://127.0.0.1:27190"; K={"Authorization":"Bearer k3y"}; NB=sys.argv[1]
async def call(m, **p):
    async with httpx.AsyncClient(timeout=120) as c:
        return (await c.post(B+"/adapter", headers=K, json={"method":m,"params":p})).json()
async def poll(after):
    async with httpx.AsyncClient(timeout=60) as c:
        return (await c.get(B+f"/notifications?after={after}", headers=K)).json()
def cells(s): return [(c["cell_id"], c["code"][:16], "unrun" if c["unrun"] else "", "ERR" if c["errored"] else "", (c["output"] or {}).get("text")) for c in s["cells"]]
async def main():
    seq=(await call("status"))["result"]["seq"]
    t=time.time(); r=await call("open", path=f"{NB}/slow.py"); print("open", round(time.time()-t,1),"s", r)
    nid=r["result"]["notebook_id"]
    print("snapshot:", cells((await call("snapshot", notebook_id=nid))["result"]))
    print("run b (wait 2s, cell sleeps 6s):", (await call("run", notebook_id=nid, cells=["vblA"], wait=True, timeout=2))["result"])
    t=time.time(); s=(await call("snapshot", notebook_id=nid))["result"]; print("snapshot during run took", round(time.time()-t,3),"s:", [(c["cell_id"],c["running"]) for c in s["cells"]])
    t=time.time(); g=await call("graph", notebook_id=nid); print("graph during run took", round(time.time()-t,2),"s", json.dumps(g)[:200])
    await asyncio.sleep(5)
    print("after run:", cells((await call("snapshot", notebook_id=nid))["result"]))
    notes=await poll(seq); print("notifications:", [(n["method"], n["seq"]) for n in notes][:14], "...", len(notes))
    print("apply wrong expected:", await call("apply", notebook_id=nid, ops=[{"op":"set","cell_id":"MJUe","code":"a = 2","expected":"a = 99"}]))
    r=await call("apply", notebook_id=nid, ops=[{"op":"set","cell_id":"MJUe","code":"a = 2","expected":"a = 1"},{"op":"insert","index":1,"code":"c = a * 3\nc"}]); print("apply:", r)
    await asyncio.sleep(0.5); print("after apply:", cells((await call("snapshot", notebook_id=nid))["result"]))
    new=r["result"]["inserted"][0]
    print("run new cell (ancestor a was edited, stale):", (await call("run", notebook_id=nid, cells=[new], wait=True, timeout=20))["result"])
    print("after:", cells((await call("snapshot", notebook_id=nid))["result"]))
    print("FILE:\n", open(f"{NB}/slow.py").read()[90:420])
    print("--- a user's pane takes over the session")
    ws=await websockets.connect(f"ws://127.0.0.1:{sys.argv[2]}/ws?session_id=pane-1&file=slow.py&access_token={sys.argv[3]}", max_size=None)
    first=[json.loads(await ws.recv()) for _ in range(2)]; print(" pane got", [m["op"] for m in first], [m["data"].get("consumer_capabilities") for m in first if m["op"]=="kernel-ready"])
    print(" agent edit after takeover:", (await call("apply", notebook_id=nid, ops=[{"op":"set","cell_id":new,"code":"c = a * 4\nc"}]))["result"])
    print(" run:", (await call("run", notebook_id=nid, cells=[new], wait=True, timeout=10))["result"], cells((await call("snapshot", notebook_id=nid))["result"])[1])
    got=[]
    try:
        while True: got.append(json.loads(await asyncio.wait_for(ws.recv(), 1))["op"])
    except asyncio.TimeoutError: pass
    print(" pane saw the agent's edit:", "notebook-document-transaction" in got, sorted(set(got)))
    print("--- plot notebook: sandbox env + png")
    t=time.time(); r=await call("open", path=f"{NB}/plot.py"); print("open plot", round(time.time()-t,1),"s")
    pid=r["result"]["notebook_id"]
    print(" run:", (await call("run", notebook_id=pid, cells=["MJUe"], wait=True, timeout=60))["result"])
    png=(await call("render_png", notebook_id=pid, cell_id="MJUe"))["result"]; print(" png bytes:", png["png"] and len(base64.b64decode(png["png"])), png["mime"])
    print(" all notebooks:", [(n["notebook_id"], len(n["cells"])) for n in (await call("snapshot"))["result"]["notebooks"]])
asyncio.run(main())
