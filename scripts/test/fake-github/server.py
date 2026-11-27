#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Test-only HTTPS stand-in for github.com's release downloads and api.github.com's
release endpoints, for the install script tests (install_test.sh, install.Tests.ps1).
Never used outside them.

    python3 -I server.py ROOT CERT KEY PORT_FILE

Serves every fake release tree ROOT/<name>/gh (the layout of scripts/test/fake-gh):
    <name>/gh/releases/<tag>/*      the release's assets
    <name>/gh/releases/<tag>/.draft       a draft (invisible without login, as on GitHub)
    <name>/gh/releases/<tag>/.prerelease  marked as a pre-release
    <name>/gh/latest                tag GitHub marks latest (only if published and not a pre-release)
    <name>/gh/order                 optional: tags newest first (default: name, descending)
    <name>/gh/ratelimit             present: every API request answers 403, rate limit exceeded
    <name>/gh/redirect-to           optional: downloads redirect to this URL prefix instead
URLs (base https://localhost:PORT/<name>):
    /<name>/api/repos/5V715/vault/releases/latest
    /<name>/api/repos/5V715/vault/releases?per_page=100
    /<name>/5V715/vault/releases/download/<tag>/<file>  -> 302 to the "CDN" below, on 127.0.0.1
    /<name>/cdn/<tag>/<file>
Every request is appended to ROOT/<name>/requests.log as "GET <path>".
Binds 127.0.0.1 on a free port and writes the port to PORT_FILE once listening.
"""
import json
import os
import socket
import ssl
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit

REPO = "5V715/vault"
ROOT = Path(sys.argv[1])


def tags_of(gh):
    rel = gh / "releases"
    if not rel.is_dir():
        return []
    order = gh / "order"
    if order.is_file():
        return [t for t in order.read_text().split() if (rel / t).is_dir()]
    return sorted((p.name for p in rel.iterdir() if p.is_dir()), reverse=True)


def release_json(gh, tag, port):
    d = gh / "releases" / tag
    assets = [{"name": p.name, "label": None, "state": "uploaded", "size": p.stat().st_size,
               "uploader": {"login": "octo", "type": "User"},
               "browser_download_url": f"https://localhost:{port}/x/{REPO}/releases/download/{tag}/{p.name}"}
              for p in sorted(d.iterdir()) if not p.name.startswith(".")]
    return {
        "url": "https://api.example/x", "id": 1,
        "author": {"login": "octo", "type": "User", "site_admin": False},
        "tag_name": tag, "target_commitish": "main",
        # Text that looks like fields, inside JSON strings: a parser must not take it.
        "name": f'{tag} "tag_name": "v9.9.9", "prerelease": false',
        "draft": (d / ".draft").exists(),
        "prerelease": (d / ".prerelease").exists(),
        "created_at": "2026-01-01T00:00:00Z",
        "assets": assets,
        "body": 'notes: {"tag_name":"v9.9.9","draft":false,"prerelease":false}',
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, code, body=b"", headers=()):
        self.send_response(code)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        url = urlsplit(self.path)
        parts = [unquote(p) for p in url.path.split("/")[1:]]
        if not parts or not parts[0] or "/" in parts[0] or parts[0].startswith("."):
            return self.send(404, b"not found")
        name, rest = parts[0], parts[1:]
        base = ROOT / name
        gh = base / "gh"
        with open(base / "requests.log", "a", encoding="utf-8") as f:
            f.write(f"GET {self.path}\n")
        port = self.server.server_address[1]
        tags = tags_of(gh)
        published = [t for t in tags if not (gh / "releases" / t / ".draft").exists()]
        if rest[:4] == ["api", "repos", "5V715", "vault"]:
            if (gh / "ratelimit").exists():
                return self.send(403, b'{"message":"API rate limit exceeded for 127.0.0.1."}',
                                 [("Content-Type", "application/json"), ("x-ratelimit-limit", "60"),
                                  ("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1999999999")])
            if rest[4:] == ["releases", "latest"]:
                latest = (gh / "latest").read_text().strip() if (gh / "latest").is_file() else ""
                if latest in published and not (gh / "releases" / latest / ".prerelease").exists():
                    return self.send(200, json.dumps(release_json(gh, latest, port), indent=2).encode(),
                                     [("Content-Type", "application/json")])
                return self.send(404, b'{"message":"Not Found"}', [("Content-Type", "application/json")])
            if rest[4:] == ["releases"]:
                body = json.dumps([release_json(gh, t, port) for t in published[:100]]).encode()
                return self.send(200, body, [("Content-Type", "application/json")])
            return self.send(404, b'{"message":"Not Found"}')
        if rest[:4] == ["5V715", "vault", "releases", "download"] and len(rest) == 6:
            tag, fname = rest[4], rest[5]
            if tag not in published or not (gh / "releases" / tag / fname).is_file() or fname.startswith("."):
                return self.send(404, b"Not Found")
            redirect = gh / "redirect-to"
            if redirect.is_file():
                target = redirect.read_text().strip() + f"/{tag}/{fname}"
            else:
                target = f"https://127.0.0.1:{port}/{name}/cdn/{tag}/{fname}"
            return self.send(302, b"", [("Location", target)])
        if rest[:1] == ["cdn"] and len(rest) == 3:
            f = gh / "releases" / rest[1] / rest[2]
            if rest[2].startswith(".") or not f.is_file():
                return self.send(404, b"Not Found")
            return self.send(200, f.read_bytes(), [("Content-Type", "application/octet-stream")])
        return self.send(404, b"Not Found")


class Server6(ThreadingHTTPServer):
    address_family = socket.AF_INET6


def main():
    cert, key, port_file = sys.argv[2], sys.argv[3], sys.argv[4]
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    ctx.load_cert_chain(cert, key)
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    port = httpd.server_address[1]
    # "localhost" may resolve to ::1 first (Windows): serve the same port there too, so a
    # client does not wait for a refused IPv6 connection. Loopback only, never "::".
    try:
        httpd6 = Server6(("::1", port), Handler)
        httpd6.socket = ctx.wrap_socket(httpd6.socket, server_side=True)
        threading.Thread(target=httpd6.serve_forever, daemon=True).start()
    except OSError:
        pass
    tmp = port_file + ".tmp"
    with open(tmp, "w") as f:
        f.write(str(port))
    os.replace(tmp, port_file)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
