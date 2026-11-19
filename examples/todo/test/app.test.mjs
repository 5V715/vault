// SPDX-License-Identifier: MIT
// The app's wiring with a fake vault-web session and a recording UI: no data call
// before Active, the screens follow the session, Closed drops pending edits.

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { TodoApp, TodoList } from "../.test-build/app.js";

const enc = new TextEncoder();
const dec = new TextDecoder();
const ID = (n) => n.toString(16).padStart(32, "0");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const settle = () => new Promise((r) => setImmediate(r));

/** A session that records every call; `store` is what is committed. */
class FakeSession {
  constructor(store = new Map()) {
    this.calls = [];
    this.store = store;
    this.buffer = [];
    this.st = "pairing";
  }
  #data(name, ...args) {
    this.calls.push([name, ...args]);
    if (this.st !== "active") return Promise.reject(new Error("vault session is not active"));
    return Promise.resolve();
  }
  state() {
    return this.st;
  }
  async confirmSas() {
    this.calls.push(["confirmSas"]);
  }
  async cancel() {
    this.calls.push(["cancel"]);
  }
  async lock() {
    this.calls.push(["lock"]);
  }
  async put(key, value) {
    await this.#data("put", key, dec.decode(value));
    this.buffer.push(["put", key, value]);
  }
  async delete(key) {
    await this.#data("delete", key);
    this.buffer.push(["delete", key]);
  }
  async list(prefix) {
    await this.#data("list", prefix);
    return [...this.store].filter(([k]) => k.startsWith(prefix)).sort().map(([key, value]) => ({ key, value }));
  }
  async commit() {
    await this.#data("commit");
    for (const [op, k, v] of this.buffer) op === "put" ? this.store.set(k, v) : this.store.delete(k);
    this.buffer = [];
  }
  dataCalls() {
    return this.calls.filter(([n]) => ["get", "put", "delete", "list", "commit"].includes(n));
  }
}

/** A UI that records the screens and keeps the latest handlers. */
class FakeUi {
  constructor() {
    this.screens = [];
  }
  get last() {
    return this.screens.at(-1);
  }
  showStart(h) {
    this.screens.push({ name: "start", h });
  }
  showOffer(offer, h) {
    this.screens.push({ name: "offer", offer, h });
  }
  showSas(sas, h) {
    this.screens.push({ name: "sas", sas, h });
  }
  showList(model, h) {
    this.screens.push({ name: "list", model: structuredClone(model), h });
  }
  showClosed(message, h) {
    this.screens.push({ name: "closed", message, h });
  }
  names() {
    return this.screens.map((s) => s.name).filter((n, i, a) => n !== a[i - 1]);
  }
}

/** A `pair` stand-in: one FakeSession per call; `cb` are the callbacks it got. */
function fakePair(store) {
  const p = async (opts) => {
    const s = new FakeSession(store);
    p.sessions.push(s);
    p.opts.push(opts);
    return s;
  };
  p.sessions = [];
  p.opts = [];
  return p;
}

function setup({ dev = false, store = new Map(), debounceMs = 30 } = {}) {
  const ui = new FakeUi();
  const pair = fakePair(store);
  const warnings = [];
  const copied = [];
  const app = new TodoApp({
    pair,
    ui,
    relayOrigin: "http://127.0.0.1:8787",
    deviceName: "test",
    dev,
    debounceMs,
    warn: (...a) => warnings.push(a.join(" ")),
    copyText: async (t) => copied.push(t),
  });
  return { app, ui, pair, warnings, copied, store };
}

/** Connect and walk the session to Active, as vault-web would report it. */
async function activate(env) {
  await env.app.connect();
  const s = env.pair.sessions.at(-1);
  const o = env.pair.opts.at(-1);
  o.onOffer({ text: "vault-pair:v1:abc", qr: null });
  o.onState("pairing");
  o.onState("awaitingApproval");
  o.onSas("123456");
  env.ui.last.h.onMatch();
  await settle();
  s.st = "active";
  o.onState("active");
  await settle();
  return { s, o };
}

describe("TodoApp", () => {
  test("no_data_call_before_active", async () => {
    const env = setup();
    env.app.start();
    assert.equal(env.ui.last.name, "start");
    env.ui.last.h.onConnect();
    await settle();
    const s = env.pair.sessions[0];
    const o = env.pair.opts[0];
    assert.equal(o.appId, "vault-todo");
    assert.equal(o.relayOrigin, "http://127.0.0.1:8787");
    assert.equal(o.testOrigin, undefined, "the page origin comes from location.origin");

    o.onOffer({ text: "vault-pair:v1:abc", qr: { size: 1, modules: new Uint8Array([1]) } });
    o.onState("pairing");
    assert.equal(env.ui.last.name, "offer");
    o.onState("awaitingApproval");
    o.onSas("123456");
    assert.equal(env.ui.last.name, "sas");
    assert.equal(env.ui.last.sas, "123456");
    await settle();
    assert.deepEqual(s.dataCalls(), [], "nothing before the user confirmed");
    env.ui.last.h.onMatch();
    await settle();
    assert.deepEqual(s.calls, [["confirmSas"]]);
    assert.deepEqual(s.dataCalls(), [], "nothing before Active either");

    s.st = "active";
    o.onState("active");
    await settle();
    assert.deepEqual(s.dataCalls(), [["list", "todo/"]], "the initial load, once, after Active");
    assert.equal(env.ui.last.name, "list");
    assert.equal(env.ui.last.model.ready, true);
    assert.deepEqual(env.ui.names(), ["start", "offer", "sas", "list"]);
  });

  test("pending_edits_dropped_on_close", async () => {
    const env = setup({ debounceMs: 50 });
    const { s, o } = await activate(env);
    env.ui.last.h.onAdd("one");
    env.ui.last.h.onAdd("two");
    assert.equal(env.ui.last.model.todos.length, 2);
    assert.equal(env.ui.last.model.status, "saving");
    s.st = "closed";
    o.onState("closed", "locked", "locked");
    await settle();
    assert.equal(env.ui.last.name, "closed");
    assert.equal(env.ui.last.message, "Vault locked.");
    await sleep(150);
    assert.deepEqual(
      s.dataCalls().filter(([n]) => n !== "list"),
      [],
      "the debounced edits never reach the session",
    );
    assert.equal(env.store.size, 0);

    // Pair again: nothing came back.
    env.ui.last.h.onPairAgain();
    const b = await activate(env);
    assert.notEqual(b.s, s, "a new session");
    assert.equal(env.ui.last.name, "list");
    assert.deepEqual(env.ui.last.model.todos, []);
  });

  test("edits are coalesced into one commit", async () => {
    const env = setup({ debounceMs: 30 });
    const { s } = await activate(env);
    env.ui.last.h.onAdd("a");
    env.ui.last.h.onAdd("b");
    const [a, b] = env.ui.last.model.todos;
    env.ui.last.h.onToggle(a.id);
    env.ui.last.h.onDelete(b.id);
    await env.app.list.idle();
    const after = s.dataCalls().slice(1);
    assert.deepEqual(
      after.map(([n, k]) => [n, k]),
      [
        ["put", `todo/${a.id}`],
        ["delete", `todo/${b.id}`],
        ["commit", undefined],
      ],
    );
    assert.equal(JSON.parse(dec.decode(env.store.get(`todo/${a.id}`))).done, true);
    assert.equal(env.ui.last.model.status, "saved");
  });

  test("wrong_app_notice", async () => {
    const env = setup();
    await env.app.connect();
    const s = env.pair.sessions[0];
    const o = env.pair.opts[0];
    o.onOffer({ text: "vault-pair:v1:abc", qr: null });
    o.onState("closed", "wrongApp", "wrong app");
    await settle();
    assert.equal(env.ui.last.name, "closed");
    assert.equal(env.ui.last.message, "This vault belongs to a different app or address.");
    assert.deepEqual(s.dataCalls(), []);
  });

  test("denied and terminated notices are text", async () => {
    const env = setup();
    await env.app.connect();
    env.pair.opts[0].onState("closed", "denied", "denied");
    await settle();
    assert.equal(env.ui.last.message, "Pairing was cancelled.");
    await activate(env);
    env.pair.opts.at(-1).onState("closed", "terminated", "terminated: <b>revoked</b>");
    await settle();
    assert.equal(env.ui.last.message, "The vault ended the session (terminated: <b>revoked</b>).");
  });

  test("cancel on the offer and code screens, lock on the list", async () => {
    const env = setup();
    await env.app.connect();
    const s = env.pair.sessions[0];
    const o = env.pair.opts[0];
    o.onOffer({ text: "t", qr: null });
    env.ui.last.h.onCancel();
    o.onSas("111111");
    env.ui.last.h.onCancel();
    await settle();
    assert.deepEqual(s.calls, [["cancel"], ["cancel"]]);

    const b = await activate(env);
    env.ui.last.h.onLock();
    await settle();
    assert.deepEqual(b.s.calls.at(-1), ["lock"]);
  });

  test("copy pairing text only in dev builds", async () => {
    const prod = setup({ dev: false });
    await prod.app.connect();
    prod.pair.opts[0].onOffer({ text: "vault-pair:v1:abc", qr: null });
    assert.equal(prod.ui.last.h.onCopy, undefined);

    const dev = setup({ dev: true });
    await dev.app.connect();
    dev.pair.opts[0].onOffer({ text: "vault-pair:v1:abc", qr: null });
    await dev.ui.last.h.onCopy();
    assert.deepEqual(dev.copied, ["vault-pair:v1:abc"]);
  });

  test("a second connect while a session is open does nothing", async () => {
    const env = setup();
    await env.app.connect();
    await env.app.connect();
    assert.equal(env.pair.sessions.length, 1);
  });

  test("events of an old session are ignored", async () => {
    const env = setup();
    await env.app.connect();
    const old = env.pair.opts[0];
    old.onState("closed", "denied", "denied");
    await settle();
    await env.app.connect();
    old.onSas("999999");
    old.onState("closed", "locked", "locked");
    await settle();
    assert.notEqual(env.ui.last.name, "sas");
    assert.notEqual(env.ui.last.message, "Vault locked.");
  });

  test("a malformed stored value is skipped with a warning, the rest loads", async () => {
    const store = new Map([
      [`todo/${ID(1)}`, enc.encode(JSON.stringify({ text: "ok", done: true, created: 1 }))],
      [`todo/${ID(2)}`, enc.encode("{not json")],
      [`todo/${ID(3)}`, enc.encode(JSON.stringify({ text: 1, done: true, created: 1 }))],
    ]);
    const env = setup({ store });
    await activate(env);
    assert.deepEqual(
      env.ui.last.model.todos.map((t) => t.text),
      ["ok"],
    );
    assert.equal(env.warnings.length, 2);
    // Still usable.
    env.ui.last.h.onAdd("more");
    await env.app.list.idle();
    assert.equal(env.store.size, 4);
  });
});

describe("TodoList", () => {
  test("saved_only_after_all_batches_committed", async () => {
    const s = new FakeSession();
    s.st = "active";
    // Commits wait until the test releases them.
    const gates = [];
    const realCommit = s.commit.bind(s);
    s.commit = () =>
      new Promise((resolve, reject) => {
        gates.push(() => realCommit().then(resolve, reject));
      });
    const statuses = [];
    const list = new TodoList(s, { debounceMs: 10, warn: () => {}, onChange: () => statuses.push(list.status) });
    await list.load();
    statuses.length = 0;
    const until = async (pred, what) => {
      for (let i = 0; i < 200 && !pred(); i++) await sleep(5);
      assert.ok(pred(), what);
    };

    list.add("a");
    await until(() => gates.length === 1, "first commit started");
    list.add("b"); // during the in-flight commit
    await sleep(40); // b's batch comes due and queues behind the running flush
    assert.equal(gates.length, 1, "flushes never overlap");
    gates[0]();
    await until(() => gates.length === 2, "second commit started");
    assert.equal(list.status, "saving", "not saved while b's commit is in flight");
    assert.ok(!statuses.includes("saved"), `no "saved" yet: ${statuses}`);
    gates[1]();
    await list.idle();
    assert.equal(list.status, "saved");
    assert.equal(s.store.size, 2);
  });

  test("a close mid-batch stops calling the session", async () => {
    const s = new FakeSession();
    s.st = "active";
    let release;
    const realPut = s.put.bind(s);
    let puts = 0;
    s.put = async (k, v) => {
      puts++;
      if (puts === 1) await new Promise((r) => (release = r));
      return realPut(k, v);
    };
    const list = new TodoList(s, { debounceMs: 10, warn: () => {} });
    await list.load();
    list.add("a");
    list.add("b");
    for (let i = 0; i < 200 && !release; i++) await sleep(5);
    assert.ok(release, "the first put is running");
    list.close();
    release();
    await list.idle();
    assert.equal(puts, 1, "no further put after close");
    assert.ok(!s.calls.some(([n]) => n === "commit"), "no commit after close");
  });

  test("edits before the load finished are ignored", async () => {
    const s = new FakeSession();
    s.st = "active";
    const list = new TodoList(s, { debounceMs: 10, warn: () => {} });
    assert.equal(list.add("early"), undefined);
    await list.load();
    assert.ok(list.add("later"));
    await list.idle();
    assert.equal(s.store.size, 1);
  });

  test("a failed commit marks the status and warns", async () => {
    const s = new FakeSession();
    s.st = "active";
    const warnings = [];
    const changes = [];
    const list = new TodoList(s, {
      debounceMs: 10,
      warn: (...a) => warnings.push(a.join(" ")),
      onChange: () => changes.push(list.status),
    });
    await list.load();
    s.commit = async () => {
      throw new Error("commit failed");
    };
    list.add("x");
    await list.idle();
    assert.equal(list.status, "error");
    assert.equal(warnings.length, 1);
  });

  test("text is capped on input", async () => {
    const s = new FakeSession();
    s.st = "active";
    const list = new TodoList(s, { debounceMs: 10, warn: () => {} });
    await list.load();
    list.add("y".repeat(10_000));
    await list.idle();
    const [value] = [...s.store.values()];
    assert.equal(JSON.parse(dec.decode(value)).text.length, 500);
  });
});
