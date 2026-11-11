# Split a page into head markup, body markup and its scripts in order, so the
# loader view never has to parse the big script bundles as HTML.
import json, sys
from html.parser import HTMLParser
class P(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=False)
        self.where = "head"; self.head = []; self.body = []; self.scripts = []; self.cur = None
    def out(self): return self.head if self.where == "head" else self.body
    def handle_starttag(self, tag, attrs):
        if tag == "body": self.where = "body"; return
        if tag in ("html", "head"): return
        if tag == "script": self.cur = {"attrs": attrs, "text": []}; return
        a = "".join(f' {k}' if v is None else f' {k}="{v.replace(chr(34), "&quot;")}"' for k, v in attrs)
        self.out().append(f"<{tag}{a}>")
    def handle_startendtag(self, tag, attrs):
        a = "".join(f' {k}' if v is None else f' {k}="{v.replace(chr(34), "&quot;")}"' for k, v in attrs)
        self.out().append(f"<{tag}{a}/>")
    def handle_endtag(self, tag):
        if tag == "script" and self.cur is not None:
            self.scripts.append({"attrs": dict(self.cur["attrs"]), "text": "".join(self.cur["text"])}); self.cur = None; return
        if tag in ("html", "head", "body"): return
        self.out().append(f"</{tag}>")
    def handle_data(self, d):
        if self.cur is not None: self.cur["text"].append(d)
        else: self.out().append(d)
    def handle_entityref(self, n): self.handle_data(f"&{n};")
    def handle_charref(self, n): self.handle_data(f"&#{n};")
    def handle_comment(self, d): pass
    def handle_decl(self, d): pass
p = P(); p.feed(open(sys.argv[1], encoding="utf-8").read()); p.close()
doc = {"head": "".join(p.head), "body": "".join(p.body), "scripts": p.scripts}
open(sys.argv[2], "w").write(json.dumps(doc))
print(len(doc["head"]), len(doc["body"]), [(s["attrs"].get("type"), len(s["text"])) for s in p.scripts])
