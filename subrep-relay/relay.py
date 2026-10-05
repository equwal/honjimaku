"""Public relay: share a live caption feed by link.

The engine runs on someone's desktop, behind NAT and without a certificate, so
viewers cannot connect to it. This is a small public service that the engine
connects *out* to, and that viewers connect *in* to.

    engine  --wss--> /pub/<room>  [relay]  /sub/<room> <--wss--  viewers
                                     |
                                     +--- /w/<room> the page they open

Viewers mine with their own Yomitan into their own Anki, so the relay never
needs Anki, audio or accounts. It moves text and nothing else - which is also
why it stays cheap to run.

Two rules shape the design:

**Viewers can never publish.** The subscriber socket ignores everything it
receives. Otherwise anyone with a share link could inject captions into
someone else's stream.

**A room id is a capability.** Anyone holding the link can read the feed, so
ids are random and long enough not to be guessed, and the publisher holds a
separate secret so nobody else can broadcast into their room.
"""

import asyncio
import json
import os
import re
import secrets
import time
from collections import deque
from html import escape

ROOM_RE = re.compile(r"^[a-zA-Z0-9_-]{6,64}$")
MAX_ROOMS = 200
MAX_VIEWERS_PER_ROOM = 200
HISTORY = 40                 # lines a joining viewer sees immediately
IDLE_ROOM_SECONDS = 900      # forget a room this long after the publisher goes


def new_room_id():
    return secrets.token_urlsafe(9)


def new_secret():
    return secrets.token_urlsafe(24)


class Room:
    def __init__(self, room_id, secret):
        self.id = room_id
        self.secret = secret
        self.viewers = set()
        self.history = deque(maxlen=HISTORY)
        self.title = ""
        self.lang = ""
        self.publisher = None
        self.last_seen = time.time()

    def snapshot(self):
        return {"type": "hello", "room": self.id, "lang": self.lang,
                "title": self.title, "viewers": len(self.viewers),
                "live": self.publisher is not None}


class Relay:
    def __init__(self):
        self.rooms = {}

    def get(self, room_id):
        return self.rooms.get(room_id)

    def reap(self):
        """Drop rooms whose publisher left and whose viewers have gone."""
        now = time.time()
        for rid, room in list(self.rooms.items()):
            if (room.publisher is None and not room.viewers
                    and now - room.last_seen > IDLE_ROOM_SECONDS):
                del self.rooms[rid]

    async def publish(self, room, payload):
        room.last_seen = time.time()
        if payload.get("type") == "final":
            room.history.append(payload)
        raw = json.dumps(payload, ensure_ascii=False)
        dead = []
        for ws in list(room.viewers):
            try:
                await ws.send(raw)
            except Exception:
                dead.append(ws)
        for ws in dead:
            room.viewers.discard(ws)


# ------------------------------------------------------------- websocket ----

async def serve_publisher(relay, ws, room_id):
    """The engine. Authenticates, then streams captions in."""
    try:
        first = await asyncio.wait_for(ws.recv(), timeout=15)
        hello = json.loads(first)
    except Exception:
        await ws.close(code=4000, reason="expected a hello")
        return

    secret = hello.get("secret") or ""
    room = relay.get(room_id)

    if room is None:
        if len(relay.rooms) >= MAX_ROOMS:
            relay.reap()
        if len(relay.rooms) >= MAX_ROOMS:
            await ws.close(code=4003, reason="relay full")
            return
        if len(secret) < 16:
            await ws.close(code=4003, reason="weak room secret")
            return
        room = Room(room_id, secret)
        relay.rooms[room_id] = room
    elif not secrets.compare_digest(room.secret, secret):
        await ws.close(code=4003, reason="wrong room secret")
        return

    if room.publisher is not None:
        # A reconnecting engine should take over rather than be refused: the
        # old socket is usually a half-dead one the relay has not noticed.
        try:
            await room.publisher.close(code=4004, reason="replaced")
        except Exception:
            pass

    room.publisher = ws
    room.lang = hello.get("lang") or room.lang
    room.title = (hello.get("title") or room.title)[:80]
    room.last_seen = time.time()
    await ws.send(json.dumps({"type": "ready", "room": room.id,
                              "viewers": len(room.viewers)}))
    await relay.publish(room, {"type": "status", "live": True,
                               "lang": room.lang, "title": room.title})

    try:
        async for message in ws:
            try:
                payload = json.loads(message)
            except ValueError:
                continue
            kind = payload.get("type")
            if kind not in ("partial", "final", "clear", "status"):
                continue
            await relay.publish(room, payload)
    except Exception:
        pass
    finally:
        if room.publisher is ws:
            room.publisher = None
            room.last_seen = time.time()
            await relay.publish(room, {"type": "status", "live": False})


async def serve_viewer(relay, ws, room_id):
    """A viewer. Read-only, by construction: nothing received is acted on."""
    room = relay.get(room_id)
    if room is None:
        await ws.send(json.dumps({"type": "gone",
                                  "error": "that stream has ended"}))
        await ws.close()
        return
    if len(room.viewers) >= MAX_VIEWERS_PER_ROOM:
        await ws.close(code=4003, reason="room is full")
        return

    room.viewers.add(ws)
    try:
        await ws.send(json.dumps(room.snapshot(), ensure_ascii=False))
        for msg in list(room.history):
            await ws.send(json.dumps(msg, ensure_ascii=False))
        # Deliberately not reading from the socket: a viewer must never be
        # able to inject a caption into someone else's feed.
        await ws.wait_closed()
    except Exception:
        pass
    finally:
        room.viewers.discard(ws)
        room.last_seen = time.time()


async def handler(relay, ws):
    path = getattr(ws, "path", None) or getattr(
        getattr(ws, "request", None), "path", "") or ""
    parts = [p for p in path.split("?")[0].split("/") if p]

    if len(parts) == 2 and ROOM_RE.match(parts[1]):
        if parts[0] == "pub":
            return await serve_publisher(relay, ws, parts[1])
        if parts[0] == "sub":
            return await serve_viewer(relay, ws, parts[1])
    await ws.close(code=4004, reason="unknown path")


# ------------------------------------------------------------------ http ----

def build_http(relay, web_dir):
    """Serves the viewer page at /w/<room> and a health check."""
    from http.server import BaseHTTPRequestHandler

    page = (web_dir / "watch.html").read_text(encoding="utf-8")

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def do_GET(self):
            path = self.path.split("?")[0]
            if path in ("/health", "/healthz"):
                return self._send(200, b'{"ok":true}', "application/json")

            parts = [p for p in path.split("/") if p]
            if len(parts) == 2 and parts[0] == "w" and ROOM_RE.match(parts[1]):
                return self._send(200, self._page(parts[1]),
                                  "text/html; charset=utf-8")

            return self._send(404, b"not found", "text/plain")

        def _page(self, room_id):
            """Fill the page, including the preview a chat client will show."""
            room = relay.get(room_id)
            live = bool(room and room.publisher is not None)
            lang = (room.lang if room else "") or ""
            name = (room.title if room else "") or "Live captions"

            title = name if not lang else "%s (%s)" % (name, lang)
            if live:
                desc = ("Live subtitles you can read along with and mine with "
                        "Yomitan. Open to follow in real time.")
            elif room:
                desc = "This stream is not broadcasting right now."
            else:
                desc = "This stream has ended."

            # The proxy knows the public host; the relay only sees localhost.
            host = self.headers.get("X-Forwarded-Host") or self.headers.get("Host", "")
            proto = self.headers.get("X-Forwarded-Proto") or "https"
            prefix = self.headers.get("X-Forwarded-Prefix", "")
            url = "%s://%s%s/w/%s" % (proto, host, prefix.rstrip("/"), room_id)

            out = page
            for token, value in (("__ROOM__", room_id),
                                 ("__OGTITLE__", title),
                                 ("__OGDESC__", desc),
                                 ("__OGURL__", url)):
                out = out.replace(token, escape(value))
            return out.encode("utf-8")

        def _send(self, code, body, ctype):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            # The page is tiny and changes with releases, not per request.
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            self.wfile.write(body)

    return Handler


async def run(host="0.0.0.0", ws_port=8811, http_port=8812, web_dir=None):
    import threading
    from http.server import ThreadingHTTPServer
    from pathlib import Path

    import websockets

    web_dir = Path(web_dir or (Path(__file__).resolve().parent / "web"))
    relay = Relay()

    srv = ThreadingHTTPServer((host, http_port), build_http(relay, web_dir))
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    async def reaper():
        while True:
            await asyncio.sleep(120)
            relay.reap()

    asyncio.ensure_future(reaper())

    async def route(ws):
        await handler(relay, ws)

    async with websockets.serve(route, host, ws_port, ping_interval=20,
                                max_size=256 * 1024):
        print("relay: ws %s:%d  http %s:%d" % (host, ws_port, host, http_port),
              flush=True)
        await asyncio.Future()


def main(argv=None):
    import argparse

    p = argparse.ArgumentParser(prog="subrep.relay")
    p.add_argument("--host", default="0.0.0.0")
    # 8801 is the licence service; keep the two able to share a host.
    p.add_argument("--ws-port", type=int, default=8811)
    p.add_argument("--http-port", type=int, default=8812)
    a = p.parse_args(argv)
    try:
        asyncio.run(run(a.host, a.ws_port, a.http_port))
    except KeyboardInterrupt:
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
