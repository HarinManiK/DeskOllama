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

import ctypes
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
LOG = os.path.join(ROOT, "harness.log")
PORT = int(os.environ.get("HARNESS_PORT", "8777"))
# Long enough for a refresh to reconnect (~200ms on localhost), short enough that a real close
# frees the memory while you are still letting go of the mouse. It cannot be zero: a refresh
# drops the socket exactly like a close does, and waiting to see whether the page comes back is
# the only thing that tells them apart.
GRACE = 1.5
# How often the held-open connection is written to. This is what actually detects a closed
# tab, since a dead socket only reveals itself when something is sent, and on Windows often
# not until the second attempt. Keep it well under the grace or detection latency, not the
# grace, becomes what you wait for.
PING = 0.25
STARTUP_WAIT = 60.0  # give up if the browser never connects at all
UNLOAD_WAIT = 15.0   # how long to wait for Ollama to actually free the memory
TAKEOVER_WAIT = 25.0 # how long to wait for a shutting-down predecessor to release the port


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
_closing = False       # past the point of no return: a relaunch must not adopt us
_logf = None

# Launched by run.bat this runs under pythonw, which has no console and no stdout. Run as
# `python serve.py` from a terminal it does, and printing is useful there.
HAS_CONSOLE = bool(getattr(sys, "stdout", None))


def say(msg):
    """Goes to the console when there is one, and always to harness.log, which is the only
    record when there is not."""
    global _logf
    line = time.strftime("%H:%M:%S ") + msg
    if HAS_CONSOLE:
        try:
            print(line, flush=True)
        except Exception:
            pass
    try:
        if _logf is None:
            # Truncate rather than grow without bound; the interesting log is this run's.
            mode = "a" if os.path.exists(LOG) and os.path.getsize(LOG) < 512 * 1024 else "w"
            _logf = open(LOG, mode, encoding="utf-8")
            _logf.write("\n=== %s ===\n" % time.strftime("%Y-%m-%d %H:%M:%S"))
        _logf.write(line + "\n")
        _logf.flush()
    except Exception:
        pass


def alert(msg):
    """A startup failure has to reach the user. Under pythonw there is no console to print
    to, so it goes up as a message box; the log always gets it either way."""
    say(msg)
    if HAS_CONSOLE:
        return
    try:
        ctypes.windll.user32.MessageBoxW(None, msg, "Local Harness", 0x10)   # MB_ICONERROR
    except Exception:
        pass


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
            # `state` is what stops a relaunch adopting a server that is already unloading:
            # the port stays bound for the whole of that, so being reachable is not the same
            # as being usable.
            return self.send_json({"launcher": True, "grace": GRACE,
                                   "state": "closing" if _closing else "running"})
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
                time.sleep(PING)
        except Exception:
            pass
        finally:
            with _lock:
                _clients -= 1
                n = _clients
                if _clients == 0:
                    _empty_since = time.monotonic()
            say("page closed (%d open)" % n)


def launcher_state():
    """"running", "closing", or None when the port belongs to something that isn't us."""
    try:
        with urllib.request.urlopen(
            "http://127.0.0.1:%d/__launcher" % PORT, timeout=1
        ) as r:
            data = json.loads(r.read())
        return data.get("state", "running") if data.get("launcher") else None
    except Exception:
        return None


def bind_server(wait=0.0):
    deadline = time.monotonic() + wait
    while True:
        try:
            return http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
        except OSError:
            if time.monotonic() >= deadline:
                return None
            time.sleep(0.25)


def main():
    global _closing
    url = "http://localhost:%d/index.html" % PORT

    httpd = bind_server()
    if httpd is None:
        state = launcher_state()
        if state == "running":
            say("Local Harness is already running. Opening that session.")
            webbrowser.open(url)
            return 0
        if state == "closing":
            # It still answers, but it is partway through unloading and about to exit.
            # Adopting it would hand the browser a server that dies a second later.
            say("the previous session is shutting down, waiting for it to finish")
            httpd = bind_server(TAKEOVER_WAIT)
        if httpd is None:
            alert("Port %d is already in use by something that is not Local Harness, so the "
                  "server could not start.\n\nClose whatever is using it, or set HARNESS_PORT "
                  "to another port (for example: set HARNESS_PORT=8890)." % PORT)
            return 1

    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()

    say("Local Harness")
    say("  serving  %s" % url)
    say("  ollama   %s" % OLLAMA)
    say("  log      %s" % LOG)
    if not resident_models():
        say("(ollama has nothing loaded right now)")
    webbrowser.open(url)
    say("opening your browser. close the tab to unload and quit.")

    started = time.monotonic()
    try:
        while True:
            time.sleep(0.25)
            with _lock:
                seen, empty = _seen_client, _empty_since
            if not seen:
                if time.monotonic() - started > STARTUP_WAIT:
                    alert("The page never connected, so Local Harness is shutting down.\n\n"
                          "If your browser did not open, try %s by hand." % url)
                    return 1
                continue
            # A refresh reconnects within the grace window, so only a real close
            # gets past here.
            if empty is not None and time.monotonic() - empty >= GRACE:
                break
    except KeyboardInterrupt:
        say("interrupted.")

    _closing = True   # from here on a relaunch must wait us out rather than adopt us
    if _unload_wanted:
        unload_all()
    else:
        say("'unload when the app closes' is off, leaving models loaded")
    say("bye.")
    return 0


if __name__ == "__main__":
    code = main()
    # Only meaningful when run from a terminal: it keeps the error on screen. Under pythonw
    # there is nobody to press Enter, and alert() has already shown a message box.
    if code and HAS_CONSOLE:
        try:
            input("\npress Enter to close ")
        except Exception:
            pass
    sys.exit(code)
