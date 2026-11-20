// SPDX-License-Identifier: MIT
// The runtime relay configuration of image builds (build.mjs --runtime-config):
// the page reads /config.json from vault-relay and accepts only a wss: relay
// origin (ws: only for loopback, and never for an https page).

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { loadRuntimeRelay, relayFromConfig } from "../.test-build/config.js";

describe("relayFromConfig", () => {
  test("a wss origin becomes the https origin vault-web dials", () => {
    assert.equal(relayFromConfig({ relay: "wss://todo.example" }, "https:"), "https://todo.example");
    assert.equal(relayFromConfig({ relay: "wss://relay.example:8443" }, "https:"), "https://relay.example:8443");
    assert.equal(relayFromConfig({ relay: "wss://[2001:db8::1]:8443" }, "https:"), "https://[2001:db8::1]:8443");
  });

  test("ws only for loopback, and only for an http page", () => {
    for (const r of ["ws://localhost:8080", "ws://127.0.0.1:8787", "ws://[::1]:8787"]) {
      assert.equal(relayFromConfig({ relay: r }, "http:"), r.replace(/^ws:/, "http:"), r);
      assert.throws(() => relayFromConfig({ relay: r }, "https:"), /https page/, r);
    }
    for (const r of ["ws://todo.example", "ws://192.168.1.20:8787", "ws://localhost.evil.example"]) {
      assert.throws(() => relayFromConfig({ relay: r }, "http:"), /loopback/, r);
    }
  });

  test("anything else is refused", () => {
    const bad = [
      "https://todo.example",
      "http://127.0.0.1:8787",
      "wss://todo.example/",
      "wss://todo.example/mailbox",
      "wss://todo.example?x",
      "wss://todo.example#x",
      "wss://user@todo.example",
      "wss://user:pw@todo.example",
      "wss://Todo.Example",
      "wss://todo.example:443",
      "wss://todo.example; script-src *",
      "wss://todo example",
      "wss://",
      "javascript:alert(1)",
      "",
      " wss://todo.example",
    ];
    for (const relay of bad) {
      assert.throws(() => relayFromConfig({ relay }, "https:"), /relay/, JSON.stringify(relay));
    }
    for (const cfg of [null, undefined, "wss://todo.example", [], {}, { relay: 1 }, { relay: "wss://a.example", x: 1 }]) {
      assert.throws(() => relayFromConfig(cfg, "https:"), /config/, JSON.stringify(cfg));
    }
  });
});

describe("loadRuntimeRelay", () => {
  const response = (body, { status = 200, type = "application/json" } = {}) => ({
    ok: status >= 200 && status < 300,
    status,
    headers: { get: (h) => (h.toLowerCase() === "content-type" ? type : null) },
    text: async () => body,
  });

  test("fetches /config.json same-origin without credentials or redirects", async () => {
    const calls = [];
    const fetchFn = async (url, init) => {
      calls.push([url, init]);
      return response('{"relay":"wss://todo.example"}\n');
    };
    assert.equal(await loadRuntimeRelay(fetchFn, "https:"), "https://todo.example");
    assert.deepEqual(calls, [["/config.json", { cache: "no-store", credentials: "omit", redirect: "error" }]]);
  });

  test("a failed, non-JSON or invalid answer is an error", async () => {
    for (const r of [
      response("not found", { status: 404, type: "text/plain" }),
      response('{"relay":"wss://todo.example"}', { type: "text/html" }),
      response("{"),
      response('{"relay":"ws://todo.example"}'),
    ]) {
      await assert.rejects(loadRuntimeRelay(async () => r, "https:"), /config/);
    }
    await assert.rejects(
      loadRuntimeRelay(async () => {
        throw new TypeError("network");
      }, "https:"),
      /config/,
    );
  });
});
