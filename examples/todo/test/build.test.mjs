// SPDX-License-Identifier: MIT
// build.mjs argument checks (no build, no vault-web needed): the relay origin
// forms it accepts, and the page/relay scheme pairing (no mixed content).

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { parseArgs } from "../build.mjs";

describe("build.mjs arguments", () => {
  test("localhost dev path unchanged", () => {
    const o = parseArgs(["--relay", "http://127.0.0.1:8787", "--dev"]);
    assert.equal(o.relay, "http://127.0.0.1:8787");
    assert.equal(o.dev, true);
    assert.equal(o.pageOrigin, undefined);
  });

  test("wss:// and ws:// relays are accepted as their https/http origin", () => {
    assert.equal(parseArgs(["--relay", "wss://192.168.1.20:8787"]).relay, "https://192.168.1.20:8787");
    assert.equal(parseArgs(["--relay", "wss://relay.example"]).relay, "https://relay.example");
    assert.equal(parseArgs(["--relay", "ws://127.0.0.1:8787"]).relay, "http://127.0.0.1:8787");
    assert.throws(() => parseArgs(["--relay", "ws://192.168.1.20:8787"]), /only allowed for loopback/);
    assert.throws(() => parseArgs(["--relay", "wss://relay.example/mailbox"]), /no path/);
    assert.throws(() => parseArgs(["--relay", "ftp://relay.example"]), /scheme/);
  });

  test("build_refuses_ws_relay_for_https_page", () => {
    const page = ["--page-origin", "https://192.168.1.20:8443"];
    for (const relay of ["ws://127.0.0.1:8787", "http://127.0.0.1:8787", "ws://localhost:8787"]) {
      assert.throws(() => parseArgs(["--relay", relay, ...page]), /mixed content/, relay);
    }
    const ok = parseArgs(["--relay", "wss://192.168.1.20:8787", ...page]);
    assert.equal(ok.relay, "https://192.168.1.20:8787");
    assert.equal(ok.pageOrigin, "https://192.168.1.20:8443");
  });

  test("wss relay refused for an http page", () => {
    const page = ["--page-origin", "http://localhost:8080"];
    for (const relay of ["wss://192.168.1.20:8787", "https://relay.example"]) {
      assert.throws(() => parseArgs(["--relay", relay, ...page]), /http page/, relay);
    }
    assert.equal(parseArgs(["--relay", "ws://127.0.0.1:8787", ...page]).pageOrigin, "http://localhost:8080");
  });

  test("the page origin is checked like an origin", () => {
    for (const bad of ["https://192.168.1.20:8443/app", "http://192.168.1.20:8080", "wss://x.example", ""]) {
      assert.throws(() => parseArgs(["--relay", "https://relay.example", "--page-origin", bad]), /page origin/, bad);
    }
    assert.throws(() => parseArgs(["--relay", "https://relay.example", "--page-origin"]), /needs a value/);
  });
});
