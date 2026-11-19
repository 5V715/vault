// SPDX-License-Identifier: MIT
// End-to-end: the wasm package (built with `npm run build:wasm`, `--target nodejs`)
// against the real vault-relay, vault-dev and vaultctl binaries. Node 22's global
// WebSocket carries the mailbox, as the browser's does.

import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";

import {
  APP_ID,
  TEST_ORIGIN,
  binaries,
  qrVector,
  sleep,
  startRelay,
  withHarness,
} from "./helpers.mjs";

// Fails with a build hint if the wasm package is missing.
const { pair, qrMatrix } = await import("../dist/node.js");

const enc = new TextEncoder();
const dec = (v) => (v === undefined ? undefined : new TextDecoder().decode(v));
const T = { timeout: 180_000 };

const ctx = {};

before(async () => {
  ctx.bins = binaries();
  ctx.relay = await startRelay(ctx.bins);
});

after(() => ctx.relay?.stop());

function startPair(probe, extra = {}) {
  return pair({
    relayOrigin: ctx.relay.origin,
    appId: APP_ID,
    deviceName: "e2e test",
    testOrigin: TEST_ORIGIN,
    ...probe.callbacks(),
    ...extra,
  });
}

/** Pair with `vault` until Active: vault-dev approves (`y <n>`), the user confirms. */
async function pairActive(h, vault) {
  const probe = h.probe();
  const session = await startPair(probe);
  const offer = await probe.offer();
  const vd = h.serveVault(vault, offer.text);
  const sas = await probe.sas();
  const prompt = await vd.prompt();
  assert.equal(prompt.code, sas, "vault-dev and the client show the same code");
  vd.answer(`y ${prompt.n}`);
  await session.confirmSas();
  await probe.state("active");
  assert.equal(session.state(), "active");
  return { session, probe, vd };
}

/** Lock, and check that the client reports `locked` and vault-dev ends cleanly. */
async function lockAndWait(a) {
  await a.session.lock();
  assert.equal(a.session.state(), "closed");
  const closed = await a.probe.state("closed");
  assert.equal(closed.reason, "locked");
  const ex = await a.vd.exit();
  assert.equal(ex.code, 0, "vault-dev exit status");
  assert.match(a.vd.text(), /session ended/);
}

async function assertDataCallsRejected(session, when) {
  const re = /not active/;
  await assert.rejects(session.get("todo/x"), re, `get ${when}`);
  await assert.rejects(session.put("todo/x", enc.encode("v")), re, `put ${when}`);
  await assert.rejects(session.delete("todo/x"), re, `delete ${when}`);
  await assert.rejects(session.list("todo/"), re, `list ${when}`);
  await assert.rejects(session.commit(), re, `commit ${when}`);
}

const entries = async (session, prefix) =>
  (await session.list(prefix)).map((e) => [e.key, dec(e.value)]);

describe("vault-web e2e", () => {
  test("pair_put_commit_reconnect_get", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();

      const a = await pairActive(h, vault);
      await a.session.put("todo/a", enc.encode("first"));
      await a.session.put("todo/b", enc.encode("second"));
      await a.session.put("other/x", enc.encode("not listed"));
      await a.session.commit();
      assert.equal(dec(await a.session.get("todo/a")), "first");
      assert.deepEqual(await entries(a.session, "todo/"), [
        ["todo/a", "first"],
        ["todo/b", "second"],
      ]);
      // Never committed: gone with the session.
      await a.session.put("todo/uncommitted", enc.encode("lost"));
      await a.session.delete("todo/b");
      await lockAndWait(a);

      const b = await pairActive(h, vault);
      assert.equal(dec(await b.session.get("todo/a")), "first");
      assert.equal(await b.session.get("todo/uncommitted"), undefined);
      assert.deepEqual(await entries(b.session, "todo/"), [
        ["todo/a", "first"],
        ["todo/b", "second"],
      ]);
      assert.equal(dec(await b.session.get("other/x")), "not listed");
      await b.session.delete("todo/a");
      await b.session.commit();
      await lockAndWait(b);

      const c = await pairActive(h, vault);
      assert.deepEqual(await entries(c.session, "todo/"), [["todo/b", "second"]]);
      await lockAndWait(c);
    }),
  );

  test("data_calls_rejected_until_active", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();
      const probe = h.probe();
      const session = await startPair(probe);
      await assertDataCallsRejected(session, "while pairing");
      await assert.rejects(session.confirmSas(), /no code to confirm yet/);

      const offer = await probe.offer();
      const vd = h.serveVault(vault, offer.text);
      await probe.state("awaitingApproval");
      await assertDataCallsRejected(session, "while the code is compared");

      // The vault approves; the user has not confirmed yet.
      const prompt = await vd.prompt();
      vd.answer(`y ${prompt.n}`);
      await sleep(1_000);
      assert.equal(session.state(), "awaitingApproval");
      await assertDataCallsRejected(session, "after the vault's approval only");

      await session.confirmSas();
      await probe.state("active");
      await session.put("todo/x", enc.encode("ok"));
      assert.equal(dec(await session.get("todo/x")), "ok");

      await session.lock();
      await assertDataCallsRejected(session, "after lock");
      assert.equal((await vd.exit()).code, 0);
    }),
  );

  test("cancel_before_active_closes_denied", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();

      // While the QR is shown: closes at once, nothing reaches a vault.
      const p1 = h.probe();
      const s1 = await startPair(p1);
      await p1.offer();
      await s1.cancel();
      assert.equal((await p1.state("closed")).reason, "denied");
      await assert.rejects(s1.confirmSas(), /no code to confirm yet/);
      await assert.rejects(s1.cancel(), /no pairing in progress/);

      // While the code is compared, with the vault's prompt open.
      const p2 = h.probe();
      const s2 = await startPair(p2);
      const vd2 = h.serveVault(vault, (await p2.offer()).text);
      await p2.sas();
      await vd2.prompt();
      await s2.cancel();
      const closed2 = await p2.state("closed");
      assert.equal(closed2.reason, "denied");
      assert.equal(closed2.detail, "denied");
      assert.ok(!p2.states().some((s) => s.state === "active"), "never active");
      await assertDataCallsRejected(s2, "after cancel");
      assert.equal((await vd2.exit()).code, 0, "vault-dev ends the session");

      // After "Codes match" but before the vault approved: cancel still wins.
      const p3 = h.probe();
      const s3 = await startPair(p3);
      const vd3 = h.serveVault(vault, (await p3.offer()).text);
      await p3.sas();
      await vd3.prompt();
      await s3.confirmSas();
      await s3.cancel();
      assert.equal((await p3.state("closed")).reason, "denied");
      assert.ok(!p3.states().some((s) => s.state === "active"), "never active");
      assert.equal((await vd3.exit()).code, 0);
    }),
  );

  test("wrong_app_refused_e2e", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault({ app: "other-app" });
      const probe = h.probe();
      const session = await startPair(probe);
      const vd = h.serveVault(vault, (await probe.offer()).text);
      const closed = await probe.state("closed");
      assert.equal(closed.reason, "wrongApp");
      assert.equal(closed.detail, "wrong app");
      assert.ok(!probe.states().some((s) => s.state === "active"), "never active");
      await assertDataCallsRejected(session, "after wrong app");
      assert.equal((await vd.exit()).code, 0);
      assert.match(vd.text(), /bound to app other-app/);
      assert.ok(!vd.hasPrompted(), "the vault never asks the user");
    }),
  );

  test("lock_then_data_call_rejected", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();
      const a = await pairActive(h, vault);
      await a.session.put("todo/x", enc.encode("x"));
      await a.session.commit();
      await assert.rejects(a.session.cancel(), /active; use lock/);
      await lockAndWait(a);
      await assertDataCallsRejected(a.session, "after lock");
      await assert.rejects(a.session.confirmSas(), /no pairing in progress/);
      // A second lock is harmless.
      await a.session.lock();
      assert.equal(a.probe.states().filter((s) => s.state === "closed").length, 1);
    }),
  );

  test("qr_matrix_matches_text", T, () =>
    withHarness(ctx, async (h) => {
      const probe = h.probe();
      const session = await startPair(probe);
      const offer = await probe.offer();
      assert.match(offer.text, /^vault-pair:v1:[A-Za-z0-9_-]+$/);
      const { size, modules } = offer.qr;
      assert.ok(Number.isInteger(size), "size is an integer");
      const version = (size - 17) / 4;
      assert.ok(Number.isInteger(version) && version >= 1 && version <= 40, `size ${size}`);
      assert.ok(modules instanceof Uint8Array);
      assert.equal(modules.length, size * size);
      assert.ok(modules.every((b) => b === 0 || b === 1));
      // The smallest version whose byte-mode capacity (level M) holds the text is
      // an upper bound (the encoder may pick denser segments, never a larger code).
      const bytes = enc.encode(offer.text).length;
      assert.ok(version <= byteModeVersion(bytes), `version ${version} for ${bytes} bytes`);
      assertFinders(size, modules);
      // The same encoder as the standalone export.
      assert.deepEqual(qrMatrix(offer.text), { size, modules });
      // And the same matrix as the native build (the Rust unit test checks this
      // vector and decodes it with an independent decoder).
      const v = qrVector();
      const m = qrMatrix(v.text);
      assert.equal(m.size, v.size);
      assert.deepEqual(rows(m), v.rows);

      await session.cancel();
      await probe.state("closed");
    }),
  );
});

/** Byte-mode capacity at level M, versions 1..40 (ISO/IEC 18004 table 7). */
const BYTE_CAPACITY_M = [
  14, 26, 42, 62, 84, 106, 122, 152, 180, 213, 251, 287, 331, 362, 412, 450, 504, 560, 624, 666,
  711, 779, 857, 911, 997, 1059, 1125, 1190, 1264, 1370, 1452, 1538, 1628, 1722, 1809, 1911, 1989,
  2099, 2213, 2331,
];

function byteModeVersion(bytes) {
  const i = BYTE_CAPACITY_M.findIndex((c) => c >= bytes);
  assert.ok(i >= 0, `${bytes} bytes do not fit a QR code`);
  return i + 1;
}

function rows(m) {
  const out = [];
  for (let y = 0; y < m.size; y++) {
    let r = "";
    for (let x = 0; x < m.size; x++) r += m.modules[y * m.size + x] ? "#" : ".";
    out.push(r);
  }
  return out;
}

function assertFinders(size, modules) {
  const at = (x, y) => modules[y * size + x];
  for (const [ox, oy] of [
    [0, 0],
    [size - 7, 0],
    [0, size - 7],
  ]) {
    for (let i = 0; i < 7; i++) {
      assert.equal(at(ox + i, oy), 1);
      assert.equal(at(ox + i, oy + 6), 1);
      assert.equal(at(ox, oy + i), 1);
      assert.equal(at(ox + 6, oy + i), 1);
    }
    for (let i = 1; i < 6; i++) {
      assert.equal(at(ox + i, oy + 1), 0);
      assert.equal(at(ox + 1, oy + i), 0);
    }
    for (let y = 2; y < 5; y++) for (let x = 2; x < 5; x++) assert.equal(at(ox + x, oy + y), 1);
  }
}
