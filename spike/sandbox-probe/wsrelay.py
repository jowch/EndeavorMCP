# A tiny WebSocket client (standard library only) and a relay table, so a view
# that can't reach 127.0.0.1 can talk to Pluto through MCP tool calls.
import base64, os, socket, struct, threading, time, queue, itertools

class WS:
    def __init__(self, host, port, path):
        self.sock = socket.create_connection((host, port), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self.sock.sendall(req.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(1)
            if not chunk: raise ConnectionError("closed during handshake")
            head += chunk
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError(head.split(b"\r\n")[0].decode(errors="replace"))
        self.sock.settimeout(None)
        self.inbox = queue.Queue()
        self.closed = False
        self.lock = threading.Lock()
        threading.Thread(target=self._reader, daemon=True).start()

    def _recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            c = self.sock.recv(n - len(buf))
            if not c: raise ConnectionError("closed")
            buf += c
        return buf

    def _reader(self):
        parts = []
        try:
            while True:
                b0, b1 = self._recv_exact(2)
                fin, op, ln = b0 & 0x80, b0 & 0x0F, b1 & 0x7F
                if ln == 126: ln = struct.unpack(">H", self._recv_exact(2))[0]
                elif ln == 127: ln = struct.unpack(">Q", self._recv_exact(8))[0]
                mask = self._recv_exact(4) if b1 & 0x80 else None
                data = self._recv_exact(ln)
                if mask: data = bytes(x ^ mask[i % 4] for i, x in enumerate(data))
                if op == 8: break
                if op == 9: self._send(10, data); continue
                if op == 10: continue
                parts.append(data)
                if fin:
                    self.inbox.put(b"".join(parts)); parts = []
        except Exception:
            pass
        self.closed = True
        self.inbox.put(None)

    def _send(self, op, data):
        hdr = bytes([0x80 | op])
        n = len(data)
        if n < 126: hdr += bytes([0x80 | n])
        elif n < 65536: hdr += bytes([0x80 | 126]) + struct.pack(">H", n)
        else: hdr += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = os.urandom(4)
        body = bytes(x ^ mask[i % 4] for i, x in enumerate(data))
        with self.lock:
            self.sock.sendall(hdr + mask + body)

    def send(self, data): self._send(2, data)

    def recv_batch(self, wait_s, max_bytes=4_000_000):
        out, size = [], 0
        try:
            first = self.inbox.get(timeout=wait_s)
        except queue.Empty:
            return out, self.closed
        if first is None: return out, True
        out.append(first); size += len(first)
        while size < max_bytes:
            try: m = self.inbox.get_nowait()
            except queue.Empty: break
            if m is None: return out, True
            out.append(m); size += len(m)
        return out, self.closed

SOCKETS = {}
_ids = itertools.count(1)

def ws_open(host, port, path):
    i = next(_ids); SOCKETS[i] = WS(host, port, path); return i

def ws_send(i, b64): SOCKETS[i].send(base64.b64decode(b64))

def ws_recv(i, wait_ms):
    msgs, closed = SOCKETS[i].recv_batch(wait_ms / 1000)
    return [base64.b64encode(m).decode() for m in msgs], closed
