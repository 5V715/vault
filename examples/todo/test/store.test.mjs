// SPDX-License-Identifier: MIT
// The pure state logic: parsing stored entries, applying edits, the commit debouncer.

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import {
  Debouncer,
  MAX_TEXT,
  applyEdit,
  closeMessage,
  coalesceWrites,
  emptyState,
  isTodoId,
  keyOf,
  newId,
  parseEntries,
} from "../.test-build/store.js";

const enc = new TextEncoder();
const ID = (n) => n.toString(16).padStart(32, "0");
const entry = (key, value) => ({
  key,
  value: value instanceof Uint8Array ? value : enc.encode(typeof value === "string" ? value : JSON.stringify(value)),
});
const settle = () => new Promise((r) => setImmediate(r));

describe("parseEntries", () => {
  test("malformed_value_skipped", () => {
    const warnings = [];
    const warn = (...a) => warnings.push(a.join(" "));
    const good = { text: "milk", done: false, created: 1 };
    const { todos, skipped } = parseEntries(
      [
        entry(`todo/${ID(1)}`, good),
        entry(`todo/${ID(2)}`, "not json {"),
        entry(`todo/${ID(3)}`, { text: 7, done: false, created: 1 }),
        entry(`todo/${ID(4)}`, { text: "x", done: "yes", created: 1 }),
        entry(`todo/${ID(5)}`, { text: "x", done: false, created: "1" }),
        entry(`todo/${ID(6)}`, { text: "x", done: false, created: Number.NaN }),
        entry(`todo/${ID(7)}`, [1, 2]),
        entry(`todo/${ID(8)}`, null),
        entry(`todo/${ID(9)}`, { text: "x".repeat(MAX_TEXT + 1), done: false, created: 1 }),
        entry(`todo/${ID(10)}`, new Uint8Array([0xff, 0xfe, 0x00])),
        entry("todo/not-an-id", good),
        entry(`todo/${ID(11).toUpperCase().replace(/0/g, "A")}`, good),
        entry(`todo/${ID(12)}/x`, good),
      ],
      warn,
    );
    assert.deepEqual(todos, [{ id: ID(1), ...good }]);
    assert.equal(skipped, 12);
    assert.equal(warnings.length, 12, "one warning per skipped entry");
    assert.ok(warnings.every((w) => w.includes("todo/")), "the warning names the key");
    assert.ok(!warnings.some((w) => w.includes("not json")), "the warning does not print the value");
  });

  test("sorted_by_created", () => {
    const { todos } = parseEntries([
      entry(`todo/${ID(1)}`, { text: "c", done: false, created: 30 }),
      entry(`todo/${ID(2)}`, { text: "a", done: true, created: 10 }),
      entry(`todo/${ID(4)}`, { text: "b2", done: false, created: 20 }),
      entry(`todo/${ID(3)}`, { text: "b1", done: false, created: 20 }),
    ]);
    assert.deepEqual(
      todos.map((t) => t.text),
      ["a", "b1", "b2", "c"],
      "by created, then by id",
    );
  });

  test("extra fields are ignored, not kept", () => {
    const { todos, skipped } = parseEntries([
      entry(`todo/${ID(1)}`, { text: "a", done: false, created: 1, extra: "<b>" }),
    ]);
    assert.equal(skipped, 0);
    assert.deepEqual(Object.keys(todos[0]).sort(), ["created", "done", "id", "text"]);
  });
});

describe("applyEdit", () => {
  test("text_capped_at_500", () => {
    const hostile = `<img src=x onerror=alert(1)><script>alert(2)</script>${"A".repeat(10_000)}`;
    const { state, writes } = applyEdit(emptyState(), { kind: "add", id: ID(1), text: hostile, created: 5 });
    assert.equal([...state.todos[0].text].length, MAX_TEXT);
    assert.equal(state.todos[0].text, hostile.slice(0, MAX_TEXT));
    assert.equal(writes.length, 1);
    assert.equal(writes[0].op, "put");
    assert.equal(writes[0].key, `todo/${ID(1)}`);
    const stored = JSON.parse(writes[0].value);
    assert.deepEqual(stored, { text: hostile.slice(0, MAX_TEXT), done: false, created: 5 });
    // What was written parses back.
    const back = parseEntries([entry(writes[0].key, writes[0].value)]);
    assert.equal(back.skipped, 0);
    assert.deepEqual(back.todos, state.todos);
  });

  test("the cap counts characters, never splitting a surrogate pair", () => {
    const text = "\u{1F600}".repeat(MAX_TEXT + 3);
    const { state } = applyEdit(emptyState(), { kind: "add", id: ID(1), text, created: 1 });
    assert.equal([...state.todos[0].text].length, MAX_TEXT);
    assert.equal(state.todos[0].text, "\u{1F600}".repeat(MAX_TEXT));
  });

  test("blank text and bad ids add nothing", () => {
    for (const edit of [
      { kind: "add", id: ID(1), text: "   ", created: 1 },
      { kind: "add", id: "zz", text: "x", created: 1 },
      { kind: "add", id: ID(1), text: "x", created: Number.POSITIVE_INFINITY },
    ]) {
      const r = applyEdit(emptyState(), edit);
      assert.deepEqual(r.state.todos, []);
      assert.deepEqual(r.writes, []);
    }
  });

  test("add, toggle and delete produce the matching writes", () => {
    let s = emptyState();
    let r = applyEdit(s, { kind: "add", id: ID(2), text: " b ", created: 2 });
    s = r.state;
    r = applyEdit(s, { kind: "add", id: ID(1), text: "a", created: 1 });
    s = r.state;
    assert.deepEqual(
      s.todos.map((t) => [t.text, t.created]),
      [
        ["a", 1],
        ["b", 2],
      ],
    );
    r = applyEdit(s, { kind: "toggle", id: ID(2) });
    assert.equal(r.state.todos[1].done, true);
    assert.deepEqual(r.writes, [
      { op: "put", key: `todo/${ID(2)}`, value: JSON.stringify({ text: "b", done: true, created: 2 }) },
    ]);
    assert.equal(s.todos[1].done, false, "the old state is not mutated");
    s = r.state;
    r = applyEdit(s, { kind: "delete", id: ID(1) });
    assert.deepEqual(
      r.state.todos.map((t) => t.id),
      [ID(2)],
    );
    assert.deepEqual(r.writes, [{ op: "delete", key: `todo/${ID(1)}` }]);
    // Unknown ids change nothing.
    assert.deepEqual(applyEdit(s, { kind: "toggle", id: ID(9) }), { state: s, writes: [] });
    assert.deepEqual(applyEdit(s, { kind: "delete", id: ID(9) }), { state: s, writes: [] });
  });

  test("coalesceWrites keeps the last write per key, in order", () => {
    const w = coalesceWrites([
      { op: "put", key: "todo/a", value: "1" },
      { op: "put", key: "todo/b", value: "1" },
      { op: "put", key: "todo/a", value: "2" },
      { op: "delete", key: "todo/b" },
    ]);
    assert.deepEqual(w, [
      { op: "put", key: "todo/a", value: "2" },
      { op: "delete", key: "todo/b" },
    ]);
  });
});

describe("ids and keys", () => {
  test("newId is 16 random bytes as hex", () => {
    const a = newId();
    assert.ok(isTodoId(a), a);
    assert.notEqual(a, newId());
    assert.equal(keyOf(a), `todo/${a}`);
    assert.equal(newId(() => new Uint8Array(16).fill(0xab)), "ab".repeat(16));
  });
});

describe("Debouncer", () => {
  test("debounce_coalesces_edits", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const flushed = [];
    const d = new Debouncer(500, (items) => {
      flushed.push(items);
    });
    d.push(1);
    t.mock.timers.tick(499);
    d.push(2);
    t.mock.timers.tick(499);
    d.push(3);
    t.mock.timers.tick(499);
    await settle();
    assert.deepEqual(flushed, [], "nothing before 500 ms of quiet");
    assert.equal(d.pending, 3);
    assert.equal(d.busy, true);
    t.mock.timers.tick(1);
    await settle();
    assert.deepEqual(flushed, [[1, 2, 3]], "one flush for the three edits");
    assert.equal(d.pending, 0);
    assert.equal(d.busy, false);
  });

  test("a flush in progress delays the next one", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const log = [];
    let release;
    const d = new Debouncer(500, async (items) => {
      log.push(`start ${items}`);
      if (items[0] === 1) await new Promise((r) => (release = r));
      log.push(`end ${items}`);
    });
    d.push(1);
    t.mock.timers.tick(500);
    await settle();
    d.push(2);
    t.mock.timers.tick(500);
    await settle();
    assert.deepEqual(log, ["start 1"], "the second flush waits for the first");
    assert.equal(d.busy, true);
    release();
    await d.idle();
    assert.deepEqual(log, ["start 1", "end 1", "start 2", "end 2"]);
  });

  test("pending_edits_dropped_on_close (debouncer)", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const flushed = [];
    const d = new Debouncer(500, (items) => {
      flushed.push(items);
    });
    d.push("a");
    d.push("b");
    t.mock.timers.tick(200);
    assert.equal(d.drop(), 2);
    assert.equal(d.pending, 0);
    t.mock.timers.tick(10_000);
    await settle();
    await d.idle();
    assert.deepEqual(flushed, [], "dropped edits are never flushed");
  });

  test("a failing flush reports the error and the debouncer keeps working", async () => {
    const errors = [];
    const flushed = [];
    const d = new Debouncer(
      5,
      (items) => {
        if (items[0] === "bad") throw new Error("boom");
        flushed.push(items);
      },
      (e) => errors.push(e.message),
    );
    d.push("bad");
    await d.idle();
    d.push("good");
    await d.idle();
    assert.deepEqual(errors, ["boom"]);
    assert.deepEqual(flushed, [["good"]]);
  });

  test("rejects a bad delay", () => {
    assert.throws(() => new Debouncer(-1, () => {}), RangeError);
  });
});

describe("closeMessage", () => {
  test("every close reason has a notice", () => {
    const reasons = [
      "denied",
      "locked",
      "terminated",
      "disconnected",
      "cancelled",
      "commitFailed",
      "sendFailed",
      "protocolError",
      "closed",
      "relayUnreachable",
      "pairingFailed",
      "keepaliveFailed",
      "keepaliveTimedOut",
      "failed",
      "wrongApp",
    ];
    for (const r of reasons) {
      const m = closeMessage(r, r);
      assert.equal(typeof m, "string");
      assert.ok(m.length > 0, r);
    }
    assert.equal(closeMessage("wrongApp", "wrong app"), "This vault belongs to a different app or address.");
    assert.equal(closeMessage("denied", "denied"), "Pairing was cancelled.");
    assert.match(closeMessage("terminated", "terminated: revoked"), /ended the session.*terminated: revoked/);
    assert.match(closeMessage(undefined, ""), /Disconnected/);
  });
});
