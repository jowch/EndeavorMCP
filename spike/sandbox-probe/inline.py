import re, base64, sys
s = open(sys.argv[1]).read()
def dec(u):
    head, data = u.split(',', 1)
    return base64.b64decode(data).decode('utf8') if 'base64' in head else __import__('urllib.parse').parse.unquote(data)
def script(m):
    attrs, url = m.group(1), m.group(2)
    attrs = re.sub(r'\s*src="[^"]*"', '', m.group(0).split('>')[0])
    body = dec(url).replace('</script', '<\\/script')
    return attrs + '>' + body + '</script>'
s = re.sub(r'<script([^>]*?)src="(data:text/javascript[^"]*)"[^>]*>\s*</script>', script, s)
s = re.sub(r'<link rel="stylesheet" href="(data:text/css[^"]*)"[^>]*>', lambda m: '<style>' + dec(m.group(1)) + '</style>', s)
open(sys.argv[2], 'w').write(s)
print(len(s))
# Feed the statefile through Pluto's injected-environment hook instead of fetch(data:)
m = re.search(r'window\.pluto_statefile = "data:;base64,([^"]*)";', s)
b64 = m.group(1)
s = s.replace(m.group(0), 'window.pluto_statefile = "injected";')
shim = '<script>(function(){const F=window.Function;let n=0;const NF=function(...a){const body=a.pop();const id="__pluto_fn_"+(n++);const el=document.createElement("script");el.textContent="window."+id+"=function("+a.join(",")+"){"+body+"\\n}";document.head.appendChild(el);el.remove();const f=window[id];delete window[id];if(typeof f!=="function")throw new EvalError("inline script blocked");return f;};NF.prototype=F.prototype;window.Function=NF;})();(function(){try{window.localStorage.length}catch(e){const m=new Map();const st={getItem:k=>m.has(k)?m.get(k):null,setItem:(k,v)=>m.set(k,String(v)),removeItem:k=>m.delete(k),clear:()=>m.clear(),key:i=>[...m.keys()][i]??null,get length(){return m.size}};Object.defineProperty(window,"localStorage",{value:st});Object.defineProperty(window,"sessionStorage",{value:st});}})();window.__state_b64="' + b64 + '";window.pluto_injected_environment={custom_get_statefile:(read,without_path,unpack)=>async(lp,setp)=>{const bin=atob(window.__state_b64);const a=new Uint8Array(bin.length);for(let i=0;i<bin.length;i++)a[i]=bin.charCodeAt(i);return without_path(unpack(a));}};</script>'
s = s.replace('<head>', '<head>' + shim, 1)
open(sys.argv[2], 'w').write(s)
