#!/usr/bin/env python3
"""Prueba de estrés del servidor de Encaje (sólo biblioteca estándar).

Contra un servidor real o uno arrancado con `encaje --fuente-prueba`:

* visores de vídeo que confirman cada frame (como el visor web),
* visores lentos que leen con retraso y no confirman (enlace saturado),
* visores que se conectan y desconectan sin parar,
* conexiones de entrada que mandan movimientos a 1000 Hz, peticiones de
  tamaño/frecuencia y basura (JSON roto, binario, mensajes enormes),

mientras vigila que el proceso siga vivo y que su memoria no crezca.

Uso:
  tests/stress/stress.py --url ws://127.0.0.1:8787 --pid $(pgrep -f fuente-prueba) \
      --seconds 60 --viewers 6 --slow 2 --churn 4 --input 4

Sale con 0 si todo fue bien y 1 si algo falló (lo explica al final).
"""

import argparse
import asyncio
import base64
import json
import os
import random
import statistics
import struct
import sys
import time
from urllib.parse import urlparse


# ── Cliente WebSocket mínimo (RFC 6455) ────────────────────────────────────

class WS:
    def __init__(self, reader, writer):
        self.r, self.w = reader, writer

    @classmethod
    async def connect(cls, url, timeout=5.0):
        u = urlparse(url)
        reader, writer = await asyncio.wait_for(
            asyncio.open_connection(u.hostname, u.port or 80), timeout)
        sock = writer.get_extra_info("socket")
        if sock is not None:
            import socket
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        key = base64.b64encode(os.urandom(16)).decode()
        writer.write((f"GET {u.path or '/'} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\n"
                      f"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                      "Sec-WebSocket-Version: 13\r\n\r\n").encode())
        await writer.drain()
        head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), timeout)
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError(head.split(b"\r\n", 1)[0].decode(errors="replace"))
        return cls(reader, writer)

    def _frame(self, opcode, payload):
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            head = struct.pack("!BB", 0x80 | opcode, 0x80 | n)
        elif n < 65536:
            head = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            head = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, n)
        # XOR rápido con enteros grandes en vez de byte a byte.
        m = (mask * (n // 4 + 1))[:n]
        body = (int.from_bytes(payload, "big") ^ int.from_bytes(m, "big")).to_bytes(n, "big") if n else b""
        return head + mask + body

    async def send_text(self, s):
        self.w.write(self._frame(0x1, s.encode()))
        await self.w.drain()

    async def send_binary(self, b):
        self.w.write(self._frame(0x2, b))
        await self.w.drain()

    async def recv(self):
        """(opcode, payload). Responde a los ping."""
        while True:
            b0, b1 = await self.r.readexactly(2)
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack("!H", await self.r.readexactly(2))[0]
            elif n == 127:
                n = struct.unpack("!Q", await self.r.readexactly(8))[0]
            payload = await self.r.readexactly(n)
            op = b0 & 0x0F
            if op == 0x9:
                self.w.write(self._frame(0xA, payload))
                continue
            if op == 0x8:
                raise ConnectionError("cerrado por el servidor")
            return op, payload

    def close(self):
        try:
            self.w.close()
        except Exception:
            pass


# ── Vigilancia del proceso ─────────────────────────────────────────────────

def rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except FileNotFoundError:
        return None
    return 0


def threads(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/task"))
    except FileNotFoundError:
        return None


# ── Cargas ─────────────────────────────────────────────────────────────────

class Stats:
    def __init__(self):
        self.frames = 0
        self.bytes = 0
        self.keyframes = 0
        self.gaps = []          # ms entre frames
        self.errors = []
        self.connects = 0


async def viewer(url, st, stop, slow=False):
    """Visor como el web: confirma cada frame. `slow`: lee con retraso y no confirma."""
    while not stop.is_set():
        try:
            ws = await WS.connect(url + "/ws/video")
            st.connects += 1
            last = None
            while not stop.is_set():
                op, data = await asyncio.wait_for(ws.recv(), 10)
                if op != 0x2:
                    continue
                now = time.perf_counter()
                if last is not None:
                    st.gaps.append((now - last) * 1000)
                last = now
                st.frames += 1
                st.bytes += len(data)
                if data[0] & 1:
                    st.keyframes += 1
                pts = struct.unpack("!Q", data[1:9])[0]
                if slow:
                    await asyncio.sleep(0.25)
                else:
                    await ws.send_text(json.dumps({"ack": pts}))
            ws.close()
        except (ConnectionError, OSError, asyncio.IncompleteReadError, asyncio.TimeoutError) as e:
            if not stop.is_set():
                st.errors.append(f"visor: {type(e).__name__}: {e}")
                await asyncio.sleep(0.5)


async def churn(url, st, stop):
    """Entra, recibe un par de frames (o ninguno) y se va de golpe."""
    while not stop.is_set():
        try:
            ws = await WS.connect(url + "/ws/video")
            st.connects += 1
            for _ in range(random.randint(0, 3)):
                await asyncio.wait_for(ws.recv(), 5)
            ws.close()
        except (ConnectionError, OSError, asyncio.IncompleteReadError, asyncio.TimeoutError) as e:
            st.errors.append(f"churn: {type(e).__name__}: {e}")
        await asyncio.sleep(random.uniform(0.01, 0.2))


async def input_flood(url, st, stop, hz=1000, garbage=True):
    """Movimientos a `hz`, toques, teclas, cambios de tamaño y basura."""
    sizes = [(2340, 1080), (1080, 2340), (1280, 800), (1920, 1200), (99999, 5), (0, 0)]
    rates = [60, 90, 120, 59.94, 1e9, -1]
    while not stop.is_set():
        try:
            ws = await WS.connect(url + "/ws/input")
            st.connects += 1
            n = 0
            period = 1.0 / hz
            next_t = time.perf_counter()
            while not stop.is_set():
                n += 1
                r = random.random()
                if r < 0.85:
                    msg = {"t": "move", "x": random.random(), "y": random.random()}
                elif r < 0.90:
                    i = random.randint(0, 9)
                    msg = random.choice([{"t": "touch_down", "id": i, "x": .5, "y": .5},
                                         {"t": "touch_move", "id": i, "x": random.random(), "y": .3},
                                         {"t": "touch_up", "id": i}])
                elif r < 0.93:
                    msg = {"t": "key", "keysym": random.choice([0x61, 0xff0d, 0xffe1]), "down": random.random() < .5}
                elif r < 0.935:
                    w, h = random.choice(sizes)
                    msg = {"t": "resize", "w": w, "h": h, "hz": random.choice(rates)}
                else:
                    msg = {"t": "wheel", "dy": random.choice([-1, 1])}
                await ws.send_text(json.dumps(msg))
                if garbage and n % 500 == 0:
                    await ws.send_text(random.choice(['{"t":', "null", "[]", '{"t":"move","x":"a"}', "x" * 70000]))
                    await ws.send_binary(os.urandom(64))
                next_t += period
                delay = next_t - time.perf_counter()
                if delay > 0:
                    await asyncio.sleep(delay)
                else:
                    next_t = time.perf_counter()
            ws.close()
        except (ConnectionError, OSError, asyncio.IncompleteReadError, asyncio.TimeoutError) as e:
            if not stop.is_set():
                st.errors.append(f"entrada: {type(e).__name__}: {e}")
                await asyncio.sleep(0.5)


# ── Principal ──────────────────────────────────────────────────────────────

async def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", default="ws://127.0.0.1:8787")
    ap.add_argument("--pid", type=int, help="pid del servidor, para vigilar memoria e hilos")
    ap.add_argument("--seconds", type=float, default=30)
    ap.add_argument("--viewers", type=int, default=4)
    ap.add_argument("--slow", type=int, default=1)
    ap.add_argument("--churn", type=int, default=2)
    ap.add_argument("--input", type=int, default=2)
    ap.add_argument("--input-hz", type=float, default=1000)
    ap.add_argument("--min-fps", type=float, default=0, help="fallar si los visores normales reciben menos")
    ap.add_argument("--max-rss-growth-mb", type=float, default=150)
    ap.add_argument("--json", help="guardar el resultado en este archivo")
    a = ap.parse_args()

    stop = asyncio.Event()
    vst, sst, cst, ist = Stats(), Stats(), Stats(), Stats()
    tasks = [asyncio.create_task(viewer(a.url, vst, stop)) for _ in range(a.viewers)]
    tasks += [asyncio.create_task(viewer(a.url, sst, stop, slow=True)) for _ in range(a.slow)]
    tasks += [asyncio.create_task(churn(a.url, cst, stop)) for _ in range(a.churn)]
    tasks += [asyncio.create_task(input_flood(a.url, ist, stop, a.input_hz)) for _ in range(a.input)]

    t0 = time.perf_counter()
    rss = []
    problems = []
    # Calentamiento: la memoria de referencia se toma a los 5 s (codificador creado).
    while time.perf_counter() - t0 < a.seconds:
        await asyncio.sleep(1)
        if a.pid:
            r = rss_kb(a.pid)
            if r is None:
                problems.append(f"el servidor (pid {a.pid}) murió a los {time.perf_counter() - t0:.0f} s")
                break
            rss.append((time.perf_counter() - t0, r, threads(a.pid)))
        el = time.perf_counter() - t0
        print(f"\r{el:5.0f} s  visores {vst.frames / max(el, 1e-9) / max(a.viewers, 1):5.1f} fps c/u  "
              f"conexiones {vst.connects + sst.connects + cst.connects + ist.connects}  "
              + (f"RSS {rss[-1][1] / 1024:.0f} MB hilos {rss[-1][2]}" if rss else ""), end="", flush=True)
    print()
    stop.set()
    await asyncio.sleep(0.5)
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    el = time.perf_counter() - t0

    fps = vst.frames / el / max(a.viewers, 1)
    gaps = sorted(vst.gaps)
    pct = lambda p: gaps[min(len(gaps) - 1, int(len(gaps) * p))] if gaps else 0
    result = {
        "segundos": round(el, 1),
        "fps_por_visor": round(fps, 1),
        "mbit_por_visor": round(vst.bytes * 8 / el / 1e6 / max(a.viewers, 1), 2),
        "intervalo_ms": {"mediana": round(statistics.median(gaps), 2) if gaps else None,
                          "p99": round(pct(0.99), 2), "max": round(gaps[-1], 2) if gaps else None},
        "keyframes": vst.keyframes,
        "frames_visores_lentos": sst.frames,
        "conexiones": {"visores": vst.connects, "lentos": sst.connects, "churn": cst.connects, "entrada": ist.connects},
        "errores": {"visores": len(vst.errors), "lentos": len(sst.errors), "churn": len(cst.errors), "entrada": len(ist.errors)},
    }
    if rss:
        base = next((r for t, r, _ in rss if t >= 5), rss[0][1])
        result["rss_mb"] = {"inicio": round(base / 1024), "max": round(max(r for _, r, _ in rss) / 1024),
                            "final": round(rss[-1][1] / 1024)}
        result["hilos"] = {"inicio": rss[0][2], "max": max(h for *_, h in rss), "final": rss[-1][2]}
        if (rss[-1][1] - base) / 1024 > a.max_rss_growth_mb:
            problems.append(f"la memoria creció {(rss[-1][1] - base) / 1024:.0f} MB")
        if rss[-1][2] and rss[0][2] and rss[-1][2] > rss[0][2] + 20:
            problems.append(f"los hilos pasaron de {rss[0][2]} a {rss[-1][2]}")
    if a.viewers and fps < a.min_fps:
        problems.append(f"{fps:.1f} fps por visor (< {a.min_fps})")
    if a.viewers and vst.frames == 0:
        problems.append("los visores no recibieron ningún frame")
    if len(vst.errors) > a.viewers:  # alguna reconexión puntual se tolera
        problems.append(f"{len(vst.errors)} errores en visores normales: {vst.errors[:3]}")
    result["problemas"] = problems
    print(json.dumps(result, indent=2, ensure_ascii=False))
    if a.json:
        with open(a.json, "w") as f:
            json.dump(result, f, indent=2, ensure_ascii=False)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
