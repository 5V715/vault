// SPDX-License-Identifier: MIT
// Unit tests of the TS wrapper (dist/index.js) against a fake wasm module: argument
// validation and the state / close-reason mapping. No wasm build needed.

import assert from "node:assert/strict";
import { beforeEach, describe, test } from "node:test";

import {
  CLOSE_REASONS,
  MAX_KEY_BYTES,
  loadWeb,
  pair,
  qrMatrix,
  setWasm,
  toCloseReasonName,
  toStateName,
} from "../dist/index.js";

/** A fake of the wasm-bindgen module that records calls. */
function fakeWasm() {
  const fake = {
    calls: [],
    rawOpts: undefined,
    state: "pairing",
    pair(opts) {
      fake.rawOpts = opts;
      fake.calls.push(["pair"]);
      return {
        confirmSas: () => {
          fake.calls.push(["confirmSas"]);
          if (fake.state !== "awaitingApproval") throw new Error("protocol error: no code to confirm yet");
        },
        cancel: () => fake.calls.push(["cancel"]),
        lock: async () => fake.calls.push(["lock"]),
        state: () => fake.state,
        get: async (k) => {
          fake.calls.push(["get", k]);
          return new Uint8Array([1]);
        },
        put: async (k, v) => fake.calls.push(["put", k, v]),
        delete: async (k) => fake.calls.push(["delete", k]),
        list: async (p) => {
          fake.calls.push(["list", p]);
          return [{ key: `${p}a`, value: new Uint8Array([2]) }];
        },
        commit: async () => fake.calls.push(["commit"]),
      };
    },
    qrMatrix: (text) => ({ size: 21, modules: new Uint8Array(441), text }),
  };
  return fake;
}

function options(extra = {}) {
  const seen = { offers: [], sas: [], states: [] };
  return {
    seen,
    opts: {
      relayOrigin: "https://relay.example",
      appId: "vault-todo",
      deviceName: "test",
      onOffer: (o) => seen.offers.push(o),
      onSas: (s) => seen.sas.push(s),
      onState: (...a) => seen.states.push(a),
      ...extra,
    },
  };
}

describe("before the wasm is loaded", () => {
  test("pair and qrMatrix say how to load it", async () => {
    await assert.rejects(pair(options().opts), /wasm not loaded/);
    assert.throws(() => qrMatrix("x"), /wasm not loaded/);
  });

  test("setWasm refuses something that is not the module", () => {
    assert.throws(() => setWasm({}), /not a vault-web wasm module/);
    assert.throws(() => setWasm(null), /not a vault-web wasm module/);
  });
});

describe("with a fake wasm module", () => {
  let fake;
  beforeEach(() => {
    fake = fakeWasm();
    setWasm(fake);
  });

  test("pair passes the validated options through", async () => {
    const { opts } = options({ testOrigin: "http://localhost:8080" });
    await pair(opts);
    const raw = fake.rawOpts;
    assert.equal(raw.relayOrigin, "https://relay.example");
    assert.equal(raw.appId, "vault-todo");
    assert.equal(raw.deviceName, "test");
    assert.equal(raw.testOrigin, "http://localhost:8080");
    assert.deepEqual(
      Object.keys(raw).sort(),
      ["appId", "deviceName", "onOffer", "onSas", "onState", "relayOrigin", "testOrigin"],
    );
  });

  test("testOrigin is left out when not given", async () => {
    await pair(options().opts);
    assert.ok(!("testOrigin" in fake.rawOpts));
  });

  test("pair rejects bad options before calling the wasm", async () => {
    const bad = [
      [undefined, /options object required/],
      [null, /options object required/],
      [{ ...options().opts, relayOrigin: undefined }, /relayOrigin must be a non-empty string/],
      [{ ...options().opts, relayOrigin: 42 }, /relayOrigin must be a non-empty string/],
      [{ ...options().opts, appId: "" }, /appId must be a non-empty string/],
      [{ ...options().opts, deviceName: null }, /deviceName must be a non-empty string/],
      [{ ...options().opts, onOffer: "x" }, /onOffer must be a function/],
      [{ ...options().opts, onSas: undefined }, /onSas must be a function/],
      [{ ...options().opts, onState: {} }, /onState must be a function/],
      [{ ...options().opts, testOrigin: 8080 }, /testOrigin must be a string/],
    ];
    for (const [opts, re] of bad) {
      await assert.rejects(pair(opts), (e) => e instanceof TypeError && re.test(e.message));
    }
    assert.equal(fake.calls.length, 0, "the wasm was never called");
  });

  test("onOffer and onSas pass through", async () => {
    const { opts, seen } = options();
    await pair(opts);
    const offer = { text: "vault-pair:v1:x", qr: { size: 21, modules: new Uint8Array(441) } };
    fake.rawOpts.onOffer(offer);
    fake.rawOpts.onSas("123456");
    assert.deepEqual(seen.offers, [offer]);
    assert.deepEqual(seen.sas, ["123456"]);
  });

  test("onState maps states, and reasons only when closed", async () => {
    const { opts, seen } = options();
    await pair(opts);
    fake.rawOpts.onState("pairing", undefined, undefined);
    fake.rawOpts.onState("awaitingApproval", undefined, undefined);
    fake.rawOpts.onState("active", undefined, undefined);
    fake.rawOpts.onState("closed", "relayUnreachable", "relay unreachable: refused");
    assert.deepEqual(seen.states, [
      ["pairing"],
      ["awaitingApproval"],
      ["active"],
      ["closed", "relayUnreachable", "relay unreachable: refused"],
    ]);
  });

  test("onState refuses names it does not know", async () => {
    const { opts, seen } = options();
    await pair(opts);
    assert.throws(() => fake.rawOpts.onState("bogus"), /unknown session state "bogus"/);
    assert.throws(() => fake.rawOpts.onState("closed", "bogus", ""), /unknown close reason/);
    assert.throws(() => fake.rawOpts.onState("closed", undefined, ""), /unknown close reason/);
    assert.equal(seen.states.length, 0);
  });

  test("every close reason name maps to itself", () => {
    for (const r of CLOSE_REASONS) assert.equal(toCloseReasonName(r), r);
    assert.equal(CLOSE_REASONS.length, 15);
    for (const s of ["pairing", "awaitingApproval", "active", "closed"]) {
      assert.equal(toStateName(s), s);
    }
  });

  test("state() maps the raw state", async () => {
    const s = await pair(options().opts);
    assert.equal(s.state(), "pairing");
    fake.state = "awaitingApproval";
    assert.equal(s.state(), "awaitingApproval");
    fake.state = "weird";
    assert.throws(() => s.state(), /unknown session state/);
  });

  test("confirmSas and cancel return promises; a throw becomes a rejection", async () => {
    const s = await pair(options().opts);
    const p = s.confirmSas();
    assert.ok(p instanceof Promise);
    await assert.rejects(p, /no code to confirm yet/);
    fake.state = "awaitingApproval";
    await s.confirmSas();
    await s.cancel();
    await s.lock();
    assert.deepEqual(
      fake.calls.map((c) => c[0]),
      ["pair", "confirmSas", "confirmSas", "cancel", "lock"],
    );
  });

  test("data calls pass valid arguments through", async () => {
    const s = await pair(options().opts);
    const v = new Uint8Array([7, 8]);
    assert.deepEqual(await s.get("todo/a"), new Uint8Array([1]));
    await s.put("todo/a", v);
    await s.delete("todo/a");
    assert.deepEqual(await s.list("todo/"), [{ key: "todo/a", value: new Uint8Array([2]) }]);
    assert.deepEqual(await s.list(""), [{ key: "a", value: new Uint8Array([2]) }]);
    await s.commit();
    assert.deepEqual(fake.calls.slice(1), [
      ["get", "todo/a"],
      ["put", "todo/a", v],
      ["delete", "todo/a"],
      ["list", "todo/"],
      ["list", ""],
      ["commit"],
    ]);
  });

  test("data calls reject bad keys and values without calling the wasm", async () => {
    const s = await pair(options().opts);
    const long = "k".repeat(MAX_KEY_BYTES + 1);
    // Multi-byte: 342 * 3 = 1026 bytes in 342 characters.
    const longUtf8 = "€".repeat(342);
    await assert.rejects(s.get(1), TypeError);
    await assert.rejects(s.get(""), /key must not be empty/);
    await assert.rejects(s.get(long), RangeError);
    await assert.rejects(s.get(longUtf8), RangeError);
    await assert.rejects(s.put("todo/a", "text"), /value must be a Uint8Array/);
    await assert.rejects(s.put("todo/a", [1, 2]), /value must be a Uint8Array/);
    await assert.rejects(s.put(undefined, new Uint8Array(1)), TypeError);
    await assert.rejects(s.delete({}), TypeError);
    await assert.rejects(s.list(undefined), /prefix must be a string/);
    await assert.rejects(s.list(long), RangeError);
    assert.deepEqual(fake.calls, [["pair"]]);
    // The limit itself is allowed.
    await s.get("k".repeat(MAX_KEY_BYTES));
  });

  test("qrMatrix validates and delegates", () => {
    assert.throws(() => qrMatrix(5), TypeError);
    assert.equal(qrMatrix("abc").text, "abc");
  });

  test("loadWeb instantiates the web module, then uses it", async () => {
    const inits = [];
    const glue = Object.assign(fakeWasm(), { default: async (i) => inits.push(i) });
    await loadWeb(glue);
    await loadWeb(glue, "https://app.example/vault_web_bg.wasm");
    assert.deepEqual(inits, [undefined, { module_or_path: "https://app.example/vault_web_bg.wasm" }]);
    await pair(options().opts);
    assert.ok(glue.rawOpts, "pair went to the web module");
  });
});

describe("build.mjs", () => {
  test("reads the wasm-bindgen version pinned in Cargo.lock, equal to vault-web's pin", async () => {
    const { readFileSync } = await import("node:fs");
    const { lockedWasmBindgenVersion } = await import("../build.mjs");
    const root = new URL("../../../", import.meta.url);
    const v = lockedWasmBindgenVersion(readFileSync(new URL("Cargo.lock", root), "utf8"));
    assert.match(v, /^\d+\.\d+\.\d+$/);
    const manifest = readFileSync(new URL("vault-web/Cargo.toml", root), "utf8");
    assert.ok(manifest.includes(`wasm-bindgen = "=${v}"`), `vault-web pins wasm-bindgen =${v}`);
  });
});

describe("package exports", () => {
  const pkg = new URL("../", import.meta.url);
  const expected = {
    "vault-web": "dist/index.js",
    "vault-web/node": "dist/node.js",
    "vault-web/web": "pkg/web/vault_web.js",
    "vault-web/web/vault_web_bg.wasm": "pkg/web/vault_web_bg.wasm",
  };

  test("every public import path resolves to its file", () => {
    for (const [spec, file] of Object.entries(expected)) {
      assert.equal(import.meta.resolve(spec), new URL(file, pkg).href, spec);
    }
  });

  test("every import path named in the docs is exported", async () => {
    const { readFileSync } = await import("node:fs");
    const named = new Set();
    for (const f of ["index.ts", "node.ts"]) {
      const src = readFileSync(new URL(f, pkg), "utf8");
      for (const m of src.matchAll(/"(vault-web(?:\/[^"\s]*)?)"/g)) named.add(m[1]);
    }
    assert.ok(named.has("vault-web/web") && named.has("vault-web/node"), [...named].join(", "));
    for (const spec of named) {
      assert.ok(spec in expected, `${spec} is documented but not in this test's list`);
      assert.doesNotThrow(() => import.meta.resolve(spec), spec);
    }
  });

  test("internal paths stay private", () => {
    assert.throws(() => import.meta.resolve("vault-web/pkg/web/vault_web.js"), {
      code: "ERR_PACKAGE_PATH_NOT_EXPORTED",
    });
  });
});
