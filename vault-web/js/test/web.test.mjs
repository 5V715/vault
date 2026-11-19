// SPDX-License-Identifier: MIT
// The `--target web` build (pkg/web, from `npm run build:wasm`) through the public
// browser path: `import * as glue from "vault-web/web"`, `loadWeb(glue, bytes)`,
// then the API from "vault-web". Node stands in for the browser; a simulated
// `globalThis.location` checks that the page origin wins over `testOrigin`.

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { after, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";

import { pair, qrMatrix, loadWeb } from "vault-web";
import { Probe, qrVector } from "./helpers.mjs";

const wasmPath = fileURLToPath(import.meta.resolve("vault-web/web/vault_web_bg.wasm"));
const gluePath = fileURLToPath(import.meta.resolve("vault-web/web"));
for (const p of [wasmPath, gluePath]) {
  if (!existsSync(p)) throw new Error(`${p} is missing: build it with \`npm run build:wasm\``);
}
const glue = await import("vault-web/web");

// Nothing listens on the discard port: the relay is unreachable at once.
const DEAD_RELAY = "http://127.0.0.1:9";

function startPair(probe, extra) {
  return pair({ relayOrigin: DEAD_RELAY, appId: "vault-todo", deviceName: "web test", ...probe.callbacks(), ...extra });
}

before(async () => {
  await loadWeb(glue, readFileSync(wasmPath));
});

describe("vault-web --target web", () => {
  test("qrMatrix matches the native vector", () => {
    const v = qrVector();
    const m = qrMatrix(v.text);
    assert.equal(m.size, v.size);
    const rows = [];
    for (let y = 0; y < m.size; y++) {
      rows.push(Array.from(m.modules.subarray(y * m.size, (y + 1) * m.size), (b) => (b ? "#" : ".")).join(""));
    }
    assert.deepEqual(rows, v.rows);
  });

  test("an unreachable relay closes the session as relayUnreachable", async () => {
    const probe = new Probe();
    await startPair(probe, { testOrigin: "http://localhost:8080" });
    const offer = await probe.offer();
    assert.match(offer.text, /^vault-pair:v1:/);
    const closed = await probe.state("closed", 15_000);
    assert.equal(closed.reason, "relayUnreachable");
    assert.match(closed.detail, /^relay unreachable: /);
  });

  test("without location, testOrigin is required", async () => {
    assert.equal(globalThis.location, undefined);
    await assert.rejects(startPair(new Probe(), {}), /pass testOrigin/);
  });

  describe("with a simulated location", () => {
    before(() => {
      globalThis.location = { origin: "https://app.example" };
    });
    after(() => {
      delete globalThis.location;
    });

    test("a different testOrigin is refused", async () => {
      await assert.rejects(
        startPair(new Probe(), { testOrigin: "http://localhost:8080" }),
        /testOrigin is only for runtimes without location/,
      );
    });

    test("location.origin is used, and an equal testOrigin is accepted", async () => {
      for (const extra of [{}, { testOrigin: "https://APP.example:443" }]) {
        const probe = new Probe();
        await startPair(probe, extra);
        await probe.offer();
        const closed = await probe.state("closed", 15_000);
        assert.equal(closed.reason, "relayUnreachable");
      }
    });
  });
});
