#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// A tiny static server for the built page (Node built-ins only), for local runs:
//   node serve.mjs [--dir dist] [--port 8080] [--host 127.0.0.1] [--relay <origin>]
//                  [--tls-cert cert.pem --tls-key key.pem]
// With --tls-cert/--tls-key (PEM; both or neither) it serves HTTPS, e.g. for a
// phone on the LAN (scripts/showcase.sh --lan).
// Every response carries the page's CSP (connect-src = the ws:/wss: form of the
// relay origin the build was made for, from dist/.build.json, or --relay) and nosniff. Only
// regular files with a known type under the directory are served: no listings,
// no dot-files, no symlinks, nothing outside it.

import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { lstat, readFile, realpath } from "node:fs/promises";
import { createServer } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { dirname, extname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);

/**
 * `value` as a canonical origin (`https://host[:port]`, `http` only for
 * loopback), or an error. No path, query, fragment or credentials, so it is
 * safe to put into the CSP. `what` names the value in the error.
 */
export function checkOrigin(value, what = "origin") {
  const fail = (why) => {
    throw new Error(`bad ${what} ${JSON.stringify(value)}: ${why}`);
  };
  if (typeof value !== "string" || !/^[A-Za-z][A-Za-z0-9+.-]*:\/\/[^\s;,'"]+$/.test(value)) {
    fail("expected http(s)://host[:port]");
  }
  let u;
  try {
    u = new URL(value);
  } catch {
    fail("not a URL");
  }
  if (u.protocol !== "https:" && u.protocol !== "http:") fail("scheme must be https (or http for loopback)");
  if (u.protocol === "http:" && !LOOPBACK.has(u.hostname)) fail("http is only allowed for loopback hosts");
  if (u.username || u.password) fail("no credentials");
  if (u.pathname !== "/" || u.search || u.hash || /[?#]/.test(value)) fail("no path, query or fragment");
  return u.origin;
}

/**
 * The relay's origin, canonical as for `checkOrigin`. The relay may also be
 * given in its WebSocket form (`wss://host[:port]`, `ws://` only for loopback):
 * that is the same relay, and it becomes the `https:`/`http:` origin.
 */
export function checkRelayOrigin(value) {
  const asHttp =
    typeof value === "string" ? value.replace(/^wss:\/\//i, "https://").replace(/^ws:\/\//i, "http://") : value;
  try {
    return checkOrigin(asHttp, "relay origin");
  } catch (e) {
    throw new Error(e.message.replace(JSON.stringify(asHttp), JSON.stringify(value)));
  }
}

/**
 * The WebSocket origin the page dials for a relay at `relayOrigin` (vault-client
 * maps https to wss and http to ws, same host and port; `mailbox_url`).
 */
export function relayWsOrigin(relayOrigin) {
  const relay = checkRelayOrigin(relayOrigin);
  return relay.replace(/^https:/, "wss:").replace(/^http:/, "ws:");
}

/**
 * The page's Content-Security-Policy for a relay at `relayOrigin`. connect-src
 * lists the relay's ws:/wss: origin: browsers check the WebSocket URL itself, and
 * an http(s) source does not match it (Chrome blocks the dial). Nothing else is
 * fetched (the wasm is bundled), so the http(s) form is not listed.
 */
export function cspFor(relayOrigin) {
  const relay = relayWsOrigin(relayOrigin);
  return [
    "default-src 'none'",
    "script-src 'self' 'wasm-unsafe-eval'",
    "style-src 'self'",
    "img-src 'self' data:",
    `connect-src ${relay}`,
    "base-uri 'none'",
    "form-action 'none'",
    "frame-ancestors 'none'",
  ].join("; ");
}

/**
 * The same policy for index.html's <meta>, without frame-ancestors: a meta
 * policy cannot carry it, and Chrome logs an error if it is there. The header
 * (serve.mjs, or your host) is what enforces frame-ancestors.
 */
export function metaCspFor(relayOrigin) {
  return cspFor(relayOrigin)
    .split("; ")
    .filter((d) => !d.startsWith("frame-ancestors "))
    .join("; ");
}

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".map": "application/json; charset=utf-8",
};

/** The file under `root` that `url` names, or undefined (never outside `root`). */
function resolvePath(root, url) {
  if (typeof url !== "string" || !url.startsWith("/")) return undefined;
  let path = url.split(/[?#]/, 1)[0];
  try {
    path = decodeURIComponent(path);
  } catch {
    return undefined;
  }
  if (path.includes("\0") || path.includes("\\")) return undefined;
  if (path === "/") path = "/index.html";
  const segments = path.slice(1).split("/");
  // Empty segments ("//", a trailing "/") and dot segments (".", "..", ".build.json").
  if (segments.some((s) => s === "" || s.startsWith("."))) return undefined;
  const full = resolve(root, ...segments);
  if (!full.startsWith(root + sep)) return undefined;
  return full;
}

/**
 * An `http.Server` serving `root` with the page's headers; an `https.Server`
 * when `tls` (`{ cert, key }`, PEM) is given.
 */
export function createStaticServer({ root, relayOrigin, tls }) {
  const realRoot = realpathSync(resolve(root));
  const headers = {
    "Content-Security-Policy": cspFor(relayOrigin),
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "Cache-Control": "no-store",
  };

  async function fileFor(url) {
    const full = resolvePath(realRoot, url);
    if (!full) return undefined;
    const type = TYPES[extname(full).toLowerCase()];
    if (!type) return undefined;
    try {
      const st = await lstat(full);
      if (!st.isFile()) return undefined; // directories, symlinks, devices
      const real = await realpath(full);
      if (!real.startsWith(realRoot + sep)) return undefined;
      return { body: await readFile(real), type };
    } catch {
      return undefined;
    }
  }

  const handler = async (req, res) => {
    const send = (status, type, body, extra = {}) => {
      res.writeHead(status, { ...headers, "Content-Type": type, "Content-Length": body.length, ...extra });
      res.end(req.method === "HEAD" ? undefined : body);
    };
    try {
      if (req.method !== "GET" && req.method !== "HEAD") {
        send(405, "text/plain; charset=utf-8", Buffer.from("method not allowed\n"), { Allow: "GET, HEAD" });
        return;
      }
      const file = await fileFor(req.url);
      if (!file) {
        send(404, "text/plain; charset=utf-8", Buffer.from("not found\n"));
        return;
      }
      send(200, file.type, file.body);
    } catch {
      if (!res.headersSent) send(500, "text/plain; charset=utf-8", Buffer.from("error\n"));
      else res.destroy();
    }
  };
  return tls
    ? createHttpsServer({ cert: tls.cert, key: tls.key, minVersion: "TLSv1.2" }, handler)
    : createServer(handler);
}

function parseArgs(argv) {
  const opts = {
    dir: join(here, "dist"),
    port: 8080,
    host: "127.0.0.1",
    relay: undefined,
    tlsCert: undefined,
    tlsKey: undefined,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      return v;
    };
    if (a === "--dir") opts.dir = resolve(next());
    else if (a === "--port") opts.port = Number(next());
    else if (a === "--host") opts.host = next();
    else if (a === "--relay") opts.relay = next();
    else if (a === "--tls-cert") opts.tlsCert = resolve(next());
    else if (a === "--tls-key") opts.tlsKey = resolve(next());
    else throw new Error(`unknown argument ${a}`);
  }
  if (!Number.isInteger(opts.port) || opts.port < 0 || opts.port > 65535) throw new Error("bad --port");
  if ((opts.tlsCert === undefined) !== (opts.tlsKey === undefined)) {
    throw new Error("--tls-cert and --tls-key must be given together");
  }
  return opts;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (!lstatSync(opts.dir, { throwIfNoEntry: false })?.isDirectory()) {
    throw new Error(`${opts.dir} is not a directory: build first (npm run build:dev)`);
  }
  let built;
  let info;
  try {
    info = JSON.parse(readFileSync(join(opts.dir, ".build.json"), "utf8"));
    built = info.relayOrigin ?? undefined;
  } catch {
    built = undefined;
  }
  if (info?.runtimeConfig) {
    throw new Error(
      `${opts.dir} is a --runtime-config build (it reads /config.json): serve it with ` +
        "vault-relay --static-dir (docs/deploy-todo-demo.md), or rebuild with --relay",
    );
  }
  const relay = opts.relay ?? built;
  if (!relay) throw new Error("no relay origin: build with build.mjs --relay <origin>, or pass --relay");
  if (built && checkRelayOrigin(relay) !== checkRelayOrigin(built)) {
    throw new Error(`--relay ${relay} differs from the build's relay ${built}: rebuild instead`);
  }
  let tls;
  if (opts.tlsCert !== undefined) {
    // Read errors name the file only; nothing of its content is printed.
    const read = (what, path) => {
      try {
        return readFileSync(path);
      } catch (e) {
        throw new Error(`cannot read the TLS ${what} ${path}: ${e.code ?? "error"}`);
      }
    };
    tls = { cert: read("certificate", opts.tlsCert), key: read("key", opts.tlsKey) };
  }
  const server = createStaticServer({ root: opts.dir, relayOrigin: relay, tls });
  server.on("error", (e) => {
    console.error(`serve.mjs: ${e.message}`);
    process.exit(1);
  });
  server.listen(opts.port, opts.host, () => {
    const { port } = server.address();
    const host = opts.host === "127.0.0.1" ? "localhost" : opts.host;
    const scheme = tls ? "https" : "http";
    console.log(`serving ${opts.dir} at ${scheme}://${host}:${port}/ (relay ${checkRelayOrigin(relay)})`);
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (e) {
    console.error(`serve.mjs: ${e.message}`);
    process.exit(1);
  }
}
