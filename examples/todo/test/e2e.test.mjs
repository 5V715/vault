// SPDX-License-Identifier: MIT
// End-to-end: the app's wiring (TodoApp, TodoList, the store) with the real
// vault-web wasm (`vault-web/node`) against the real vault-relay, vault-dev and
// vaultctl. The screens are recorded instead of drawn; the "user" clicks through
// the handlers the app gave them. Process helpers are vault-web's own.
//
// Needs vault-web built (in vault-web/js: npm ci, npm run build, npm run build:wasm)
// and the binaries (built with cargo, or VAULT_BIN_DIR).

import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";

import {
  APP_ID,
  TEST_ORIGIN,
  binaries,
  startRelay,
  withHarness,
  withTimeout,
} from "../../../vault-web/js/test/helpers.mjs";
import { TodoApp } from "../.test-build/app.js";

// Fails with a build hint if the wasm package is missing.
const { pair } = await import("vault-web/node");

const enc = new TextEncoder();
const dec = new TextDecoder();
const T = { timeout: 180_000 };
const KEY_RE = /^todo\/[0-9a-f]{32}$/;
const WRONG_APP = "This vault belongs to a different app or address.";

const ctx = {};

before(async () => {
  ctx.bins = binaries();
  ctx.relay = await startRelay(ctx.bins);
});

after(() => ctx.relay?.stop());

/** Records the screens; `waitFor` resolves with the first matching one from `from` on. */
class RecordingUi {
  constructor() {
    this.screens = [];
    this.listeners = new Set();
  }
  #push(s) {
    this.screens.push(s);
    for (const l of this.listeners) l();
  }
  showStart(h) {
    this.#push({ name: "start", h });
  }
  showOffer(offer, h) {
    this.#push({ name: "offer", offer, h });
  }
  showSas(sas, h) {
    this.#push({ name: "sas", sas, h });
  }
  showList(model, h) {
    this.#push({ name: "list", model: structuredClone(model), h });
  }
  showClosed(message, h) {
    this.#push({ name: "closed", message, h });
  }
  get last() {
    return this.screens.at(-1);
  }
  mark() {
    return this.screens.length;
  }
  waitFor(pred, what, from = 0, ms = 30_000) {
    return withTimeout(
      new Promise((resolve) => {
        const check = () => {
          const s = this.screens.slice(from).find(pred);
          if (s) {
            this.listeners.delete(check);
            resolve(s);
          }
        };
        this.listeners.add(check);
        check();
      }),
      ms,
      `${what}; screens so far: ${this.describe()}`,
    );
  }
  describe() {
    return JSON.stringify(
      this.screens.map((s) =>
        s.name === "list"
          ? { list: s.model.todos.map((t) => `${t.text}${t.done ? " (done)" : ""}`), status: s.model.status }
          : s.name === "closed"
            ? { closed: s.message }
            : s.name,
      ),
    );
  }
}

function newApp() {
  const ui = new RecordingUi();
  const warnings = [];
  const env = { ui, warnings, session: undefined };
  const app = new TodoApp({
    // Node has no `location`: pass the page origin the vault was created for.
    // The raw session is kept so a test can end it behind the app's back.
    pair: async (opts) => (env.session = await pair({ ...opts, testOrigin: TEST_ORIGIN })),
    ui,
    relayOrigin: ctx.relay.origin,
    deviceName: "todo e2e",
    dev: false,
    warn: (...a) => warnings.push(a.map(String).join(" ")),
  });
  env.app = app;
  return env;
}

/**
 * Pair `env.app` with `vault` until the list is loaded: vault-dev approves
 * (`y <n>`), the user clicks "Codes match" after comparing the codes.
 */
async function pairApp(h, env, vault) {
  const from = env.ui.mark();
  await env.app.connect();
  const offer = await env.ui.waitFor((s) => s.name === "offer", "the QR screen", from);
  assert.ok(offer.offer.qr && offer.offer.qr.modules.length === offer.offer.qr.size ** 2, "a QR matrix");
  const vd = h.serveVault(vault, offer.offer.text);
  const sas = await env.ui.waitFor((s) => s.name === "sas", "the code screen", from);
  const prompt = await vd.prompt();
  assert.equal(prompt.code, sas.sas, "the vault and the page show the same code");
  vd.answer(`y ${prompt.n}`);
  sas.h.onMatch();
  const list = await env.ui.waitFor((s) => s.name === "list" && s.model.ready, "the loaded list", from);
  assert.ok(
    !env.ui.screens.slice(from).some((s) => s.name === "list" && s.model.todos.length > 0 && !s.model.ready),
    "nothing shown before the load",
  );
  return { vd, list };
}

/** "Lock" on the list screen; waits for the notice and vault-dev's clean exit. */
async function lockApp(env, vd, notice = "Vault locked.") {
  const from = env.ui.mark();
  env.ui.last.h.onLock();
  const closed = await env.ui.waitFor((s) => s.name === "closed", "the closed screen", from);
  assert.equal(closed.message, notice);
  assert.equal((await vd.exit()).code, 0, "vault-dev exit status");
}

const shown = (env) => env.ui.last.model.todos.map((t) => [t.text, t.done]);

/** Pairs with vault-web directly (no app), for checking and planting raw entries. */
async function rawSession(h, vault) {
  const probe = h.probe();
  const session = await pair({
    relayOrigin: ctx.relay.origin,
    appId: APP_ID,
    deviceName: "todo e2e raw",
    testOrigin: TEST_ORIGIN,
    ...probe.callbacks(),
  });
  const vd = h.serveVault(vault, (await probe.offer()).text);
  await probe.sas();
  const prompt = await vd.prompt();
  vd.answer(`y ${prompt.n}`);
  await session.confirmSas();
  await probe.state("active");
  return {
    session,
    async close() {
      await session.lock();
      assert.equal((await vd.exit()).code, 0);
    },
  };
}

describe("todo e2e", () => {
  test("todo_list_persists_e2e", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();
      const env = newApp();

      const a = await pairApp(h, env, vault);
      assert.deepEqual(a.list.model.todos, [], "a new vault is empty");
      for (const text of ["milk", "bread", "eggs"]) {
        env.ui.last.h.onAdd(text);
        // Distinct `created` values keep the order deterministic.
        await new Promise((r) => setTimeout(r, 5));
      }
      const [milk, bread, eggs] = env.ui.last.model.todos;
      assert.deepEqual(
        [milk.text, bread.text, eggs.text],
        ["milk", "bread", "eggs"],
      );
      env.ui.last.h.onToggle(bread.id);
      env.ui.last.h.onDelete(eggs.id);
      assert.equal(env.ui.last.model.status, "saving");
      await env.app.list.idle();
      assert.equal(env.ui.last.model.status, "saved");
      await lockApp(env, a.vd);
      assert.equal(env.app.list, undefined, "the list is gone with the session");

      const b = await pairApp(h, env, vault);
      assert.deepEqual(shown(env), [
        ["milk", false],
        ["bread", true],
      ]);
      await lockApp(env, b.vd);

      // The stored format, read with vault-web directly; plant a malformed value.
      const raw = await rawSession(h, vault);
      const entries = await raw.session.list("todo/");
      assert.equal(entries.length, 2);
      for (const e of entries) {
        assert.match(e.key, KEY_RE);
        const v = JSON.parse(dec.decode(e.value));
        assert.deepEqual(Object.keys(v).sort(), ["created", "done", "text"]);
        assert.equal(typeof v.created, "number");
      }
      await raw.session.put(`todo/${"f".repeat(32)}`, enc.encode("{not json"));
      await raw.session.commit();
      await raw.close();

      const c = await pairApp(h, env, vault);
      assert.deepEqual(shown(env), [
        ["milk", false],
        ["bread", true],
      ]);
      assert.ok(
        env.warnings.some((w) => w.includes(`todo/${"f".repeat(32)}`)),
        `a warning for the malformed entry: ${env.warnings}`,
      );
      await lockApp(env, c.vd);
    }),
  );

  test("pending_edits_dropped_on_close_e2e", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();
      const env = newApp();

      const a = await pairApp(h, env, vault);
      env.ui.last.h.onAdd("kept");
      await env.app.list.idle();
      env.ui.last.h.onAdd("lost");
      assert.equal(env.ui.last.model.status, "saving");
      // The session ends inside the 500 ms window without the app's Lock (as when
      // the vault locks): the edit is dropped, the list cleared, the user told.
      const from = env.ui.mark();
      await env.session.lock();
      const closed = await env.ui.waitFor((s) => s.name === "closed", "the closed screen", from);
      assert.equal(closed.message, "Vault locked. 1 change was not saved.");
      assert.equal((await a.vd.exit()).code, 0);
      assert.equal(env.app.list, undefined, "the list is cleared");
      await new Promise((r) => setTimeout(r, 800));

      const b = await pairApp(h, env, vault);
      assert.deepEqual(shown(env), [["kept", false]], "the dropped edit does not reappear");
      await lockApp(env, b.vd);
    }),
  );

  test("lock_flushes_pending_edits_first_e2e", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();
      const env = newApp();
      const a = await pairApp(h, env, vault);
      env.ui.last.h.onAdd("saved by lock");
      assert.equal(env.ui.last.model.status, "saving");
      // Lock at once, inside the 500 ms window: the app commits first.
      await lockApp(env, a.vd);
      const b = await pairApp(h, env, vault);
      assert.deepEqual(shown(env), [["saved by lock", false]]);
      await lockApp(env, b.vd);
    }),
  );

  test("wrong_app_refused_e2e", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault({ app: "other-app" });
      const env = newApp();
      await env.app.connect();
      const offer = await env.ui.waitFor((s) => s.name === "offer", "the QR screen");
      const vd = h.serveVault(vault, offer.offer.text);
      const closed = await env.ui.waitFor((s) => s.name === "closed", "the closed screen");
      assert.equal(closed.message, WRONG_APP);
      // The code may already be on screen (it comes from the handshake); the list never is.
      assert.ok(!env.ui.screens.some((s) => s.name === "list"), env.ui.describe());
      assert.equal((await vd.exit()).code, 0);
      assert.ok(!vd.hasPrompted(), "the vault never asks the user");
    }),
  );

  test("cancel_shows_notice_e2e", T, () =>
    withHarness(ctx, async (h) => {
      const vault = h.initVault();
      const env = newApp();
      await env.app.connect();
      const offer = await env.ui.waitFor((s) => s.name === "offer", "the QR screen");
      const vd = h.serveVault(vault, offer.offer.text);
      const sas = await env.ui.waitFor((s) => s.name === "sas", "the code screen");
      await vd.prompt();
      sas.h.onCancel();
      const closed = await env.ui.waitFor((s) => s.name === "closed", "the closed screen");
      assert.equal(closed.message, "Pairing was cancelled.");
      assert.ok(!env.ui.screens.some((s) => s.name === "list"), "never active");
      assert.equal((await vd.exit()).code, 0);
    }),
  );
});
