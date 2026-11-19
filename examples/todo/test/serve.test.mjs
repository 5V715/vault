// SPDX-License-Identifier: MIT
// serve.mjs: the exact CSP, nosniff, the wasm MIME type, no directory listing,
// and no way out of the served directory.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";

import { checkRelayOrigin, cspFor, createStaticServer } from "../serve.mjs";

const RELAY = "http://127.0.0.1:8787";
const CSP =
  "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; " +
  "connect-src ws://127.0.0.1:8787; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const SECRET = "TOP SECRET OUTSIDE ROOT";

const ctx = {};

before(async () => {
  ctx.base = mkdtempSync(join(tmpdir(), "todo-serve-"));
  const root = join(ctx.base, "dist");
  mkdirSync(join(root, "sub", "dir"), { recursive: true });
  writeFileSync(join(root, "index.html"), "<!doctype html><title>t</title>");
  writeFileSync(join(root, "main.js"), "console.log(1)");
  writeFileSync(join(root, "styles.css"), "body{}");
  writeFileSync(join(root, "x.wasm"), new Uint8Array([0, 0x61, 0x73, 0x6d]));
  writeFileSync(join(root, "sub", "dir", "f.js"), "1");
  writeFileSync(join(root, ".build.json"), JSON.stringify({ relayOrigin: RELAY }));
  writeFileSync(join(root, "notes.txt"), "unknown type");
  writeFileSync(join(ctx.base, "secret.txt"), SECRET);
  symlinkSync(join(ctx.base, "secret.txt"), join(root, "link.js"));
  ctx.server = createStaticServer({ root, relayOrigin: RELAY });
  await new Promise((r) => ctx.server.listen(0, "127.0.0.1", r));
  ctx.port = ctx.server.address().port;
});

after(() => {
  ctx.server?.close();
  rmSync(ctx.base, { recursive: true, force: true });
});

/** A raw request: the path is sent exactly as given (no client-side normalisation). */
function get(path, method = "GET") {
  return new Promise((resolve, reject) => {
    const req = request({ host: "127.0.0.1", port: ctx.port, path, method }, (res) => {
      let body = "";
      res.setEncoding("latin1");
      res.on("data", (d) => (body += d));
      res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, body }));
    });
    req.on("error", reject);
    req.end();
  });
}

describe("relay origin and CSP", () => {
  test("the CSP is exactly the one in the plan", () => {
    assert.equal(cspFor(RELAY), CSP);
  });

  test("connect-src is the relay's WebSocket origin (http -> ws, https -> wss)", () => {
    const connect = (relay) => cspFor(relay).match(/connect-src ([^;]+);/)[1];
    assert.equal(connect("http://127.0.0.1:8787"), "ws://127.0.0.1:8787");
    assert.equal(connect("http://localhost:8787/"), "ws://localhost:8787");
    assert.equal(connect("http://[::1]:9000"), "ws://[::1]:9000");
    assert.equal(connect("https://relay.example"), "wss://relay.example");
    assert.equal(connect("https://relay.example:8443"), "wss://relay.example:8443");
    assert.equal(connect("HTTPS://Relay.Example:443"), "wss://relay.example");
  });

  test("checkRelayOrigin accepts origins only", () => {
    assert.equal(checkRelayOrigin("https://relay.example"), "https://relay.example");
    assert.equal(checkRelayOrigin("HTTPS://Relay.Example:443/"), "https://relay.example");
    assert.equal(checkRelayOrigin("http://127.0.0.1:8787"), "http://127.0.0.1:8787");
    assert.equal(checkRelayOrigin("http://localhost:8787/"), "http://localhost:8787");
    assert.equal(checkRelayOrigin("http://[::1]:8787"), "http://[::1]:8787");
    for (const bad of [
      "",
      "relay.example",
      "https://relay.example/path",
      "https://relay.example?x=1",
      "https://relay.example#f",
      "https://u:p@relay.example",
      "http://relay.example",
      "ftp://relay.example",
      "https://relay.example; script-src *",
      "https://relay.example 'unsafe-inline'",
      "https://relay.example\nX: y",
    ]) {
      assert.throws(() => checkRelayOrigin(bad), /relay origin/, JSON.stringify(bad));
    }
  });
});

describe("serve", () => {
  test("serves the page with the CSP and nosniff", async () => {
    const r = await get("/");
    assert.equal(r.status, 200);
    assert.equal(r.headers["content-type"], "text/html; charset=utf-8");
    assert.equal(r.headers["content-security-policy"], CSP);
    assert.equal(r.headers["x-content-type-options"], "nosniff");
    assert.match(r.body, /<title>t<\/title>/);
    const idx = await get("/index.html");
    assert.equal(idx.status, 200);
  });

  test("MIME types, wasm included", async () => {
    assert.equal((await get("/main.js")).headers["content-type"], "text/javascript; charset=utf-8");
    assert.equal((await get("/styles.css")).headers["content-type"], "text/css; charset=utf-8");
    const w = await get("/x.wasm");
    assert.equal(w.status, 200);
    assert.equal(w.headers["content-type"], "application/wasm");
    assert.equal(w.headers["x-content-type-options"], "nosniff");
    assert.equal((await get("/sub/dir/f.js")).status, 200);
    assert.equal((await get("/main.js?v=1")).status, 200, "a query is ignored");
    assert.equal((await get("/notes.txt")).status, 404, "unknown types are not served");
  });

  test("no directory listing", async () => {
    for (const p of ["/sub", "/sub/", "/sub/dir/", "/missing.js"]) {
      const r = await get(p);
      assert.equal(r.status, 404, p);
      assert.ok(!r.body.includes("f.js"), `${p} lists nothing`);
      assert.equal(r.headers["content-security-policy"], CSP, `${p}: CSP on errors too`);
      assert.equal(r.headers["x-content-type-options"], "nosniff");
    }
  });

  test("serve_refuses_traversal", async () => {
    for (const p of [
      "/../secret.txt",
      "/sub/../../secret.txt",
      "/%2e%2e/secret.txt",
      "/%2E%2E%2Fsecret.txt",
      "/sub/..%2f..%2fsecret.txt",
      "/..%5csecret.txt",
      "/sub%5c..%5c..%5csecret.txt",
      "/%00",
      "/main.js%00.css",
      "//etc/passwd",
      "/%2fetc%2fpasswd",
      "/.build.json",
      "/sub/./dir/f.js",
      "/link.js",
      "/%",
      "*",
    ]) {
      const r = await get(p);
      assert.ok(r.status >= 400 && r.status < 500, `${p}: status ${r.status}`);
      assert.ok(!r.body.includes(SECRET), `${p} leaked the file outside the root`);
    }
  });

  test("only GET and HEAD", async () => {
    const h = await get("/main.js", "HEAD");
    assert.equal(h.status, 200);
    assert.equal(h.body, "");
    const p = await get("/main.js", "POST");
    assert.equal(p.status, 405);
    assert.equal(p.headers.allow, "GET, HEAD");
  });
});
