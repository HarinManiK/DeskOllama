#!/usr/bin/env python3
"""Local Harness launcher.

Serves index.html, opens it in the default browser, and holds an open connection
to the page. That connection is the liveness signal: when the tab closes the
socket drops and this process notices immediately, unloads every resident model
and exits. A browser unload handler is best-effort by spec, so it can and does
get skipped. A dropped socket cannot be.

A refresh drops the socket too, so the last page going away starts a short grace
window rather than unloading straight away. The reloading page reconnects well
inside it and nothing is unloaded.

    python serve.py            # or just double-click run.bat

Environment:
    HARNESS_PORT   port to serve on (default 8777)
    OLLAMA_HOST    where Ollama is listening (default http://localhost:11434)
"""

import http.server
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request
import webbrowser
from urllib.parse import parse_qs, urlparse

ROOT = os.path.dirname(os.path.abspath(__file__))
PORT = int(os.environ.get("HARNESS_PORT", "8777"))
GRACE = 5.0          # seconds the page has to come back before we call it closed
STARTUP_WAIT = 60.0  # give up if the browser never connects at all
UNLOAD_WAIT = 15.0   # how long to wait for Ollama to actually free the memory


def ollama_base():
    raw = os.environ.get("OLLAMA_HOST", "http://localhost:11434").strip()
    if not raw:
        raw = "http://localhost:11434"
    if not raw.startswith(("http://", "https://")):
        raw = "http://" + raw
    return raw.rstrip("/")


OLLAMA = ollama_base()

_lock = threading.Lock()
_clients = 0
_seen_client = False
_empty_since = None    # monotonic time the client count last hit zero
_unload_wanted = True  # mirrors the page's "unload when the app closes" setting


def say(msg):
    print(msg, flush=True)


# ---------------------------------------------------------------- Ollama

def api(path, payload=None, timeout=10):
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(
        OLLAMA + path, data=data, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
    return json.loads(body) if body else {}


def resident_models():
    try:
        return [m["name"] for m in api("/api/ps").get("models", [])]
    except Exception:
        return []


def unload_all():
    """keep_alive 0 with no prompt is Ollama's documented 'drop this model now'."""
    names = resident_models()
    if not names:
        say("no models were loaded")
        return
    say("unloading: " + ", ".join(names))
    for name in names:
        try:
            api("/api/generate", {"model": name, "keep_alive": 0, "stream": False})
        except Exception as e:
            say("  could not unload %s (%s)" % (name, e))

    # The request returns before the memory is actually released, so confirm.
    deadline = time.monotonic() + UNLOAD_WAIT
    while time.monotonic() < deadline:
        left = resident_models()
        if not left:
            say("all models unloaded")
            return
        time.sleep(0.4)
    say("still resident after %gs: %s" % (UNLOAD_WAIT, ", ".join(resident_models())))


# ---------------------------------------------------------------- server

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def log_message(self, *args):
        pass   # the request log would bury the status lines that matter

    def do_GET(self):
        route = urlparse(self.path)
        if route.path == "/__launcher":
            return self.send_json({"launcher": True, "grace": GRACE})
        if route.path == "/__alive":
            return self.stream_alive(parse_qs(route.query))
        return super().do_GET()

    def send_json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def stream_alive(self, query):
        """Held open for the lifetime of the page. The periodic write is what makes
        a vanished tab visible: it fails the moment the socket is gone."""
        global _clients, _seen_client, _empty_since, _unload_wanted
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        with _lock:
            _clients += 1
            _seen_client = True
            _empty_since = None
            _unload_wanted = query.get("unload", ["1"])[0] != "0"
            n = _clients
        say("page connected (%d open)" % n)

        try:
            while True:
                self.wfile.write(b": ping\n\n")
                self.wfile.flush()
                time.sleep(1)
        except Exception:
            pass
        finally:
            with _lock:
                _clients -= 1
                n = _clients
                if _clients == 0:
                    _empty_since = time.monotonic()
            say("page closed (%d open)" % n)


def existing_launcher():
    try:
        with urllib.request.urlopen(
            "http://127.0.0.1:%d/__launcher" % PORT, timeout=1
        ) as r:
            return json.loads(r.read()).get("launcher") is True
    except Exception:
        return False


def main():
    url = "http://localhost:%d/index.html" % PORT

    try:
        httpd = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    except OSError:
        if existing_launcher():
            say("Local Harness is already running. Opening that session.")
            webbrowser.open(url)
            return 0
        say("Port %d is in use by something else." % PORT)
        say("Set HARNESS_PORT to pick another, e.g.  set HARNESS_PORT=8890")
        return 1

    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()

    say("Local Harness")
    say("  serving  %s" % url)
    say("  ollama   %s" % OLLAMA)
    say("")
    if not resident_models():
        say("(ollama has nothing loaded right now)")
    webbrowser.open(url)
    say("opening your browser. close the tab to unload and quit.")
    say("")

    started = time.monotonic()
    try:
        while True:
            time.sleep(0.25)
            with _lock:
                seen, empty = _seen_client, _empty_since
            if not seen:
                if time.monotonic() - started > STARTUP_WAIT:
                    say("the page never connected. giving up.")
                    say("open %s by hand if the browser did not." % url)
                    return 1
                continue
            # A refresh reconnects within the grace window, so only a real close
            # gets past here.
            if empty is not None and time.monotonic() - empty >= GRACE:
                break
    except KeyboardInterrupt:
        say("")
        say("interrupted.")

    say("")
    if _unload_wanted:
        unload_all()
    else:
        say("'unload when the app closes' is off, leaving models loaded")
    say("bye.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
