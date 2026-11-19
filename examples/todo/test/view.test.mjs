// SPDX-License-Identifier: MIT
// The view on a tiny DOM shim (test/dom-shim.mjs): text goes in as text, the
// screens show what docs/integration.md asks for, and the QR is drawn from the matrix.

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { ShimDocument } from "./dom-shim.mjs";
import { MAX_TEXT, applyEdit, emptyState } from "../.test-build/store.js";
import { createUi, renderQr, renderTodoList } from "../.test-build/view.js";

const ID = (n) => n.toString(16).padStart(32, "0");
const SVG = "http://www.w3.org/2000/svg";
const noop = () => {};
const listHandlers = (log = []) => ({
  onAdd: (t) => log.push(["add", t]),
  onToggle: (id) => log.push(["toggle", id]),
  onDelete: (id) => log.push(["delete", id]),
  onLock: () => log.push(["lock"]),
});

function setup(opts = { dev: false }) {
  const doc = new ShimDocument();
  const root = doc.createElement("main");
  return { doc, root, ui: createUi(doc, root, opts) };
}

const HOSTILE = `<img src=x onerror="alert(1)"><script>alert(2)</script>&lt;b&gt;${"‮".repeat(3)}${"X".repeat(10_000)}`;

describe("view", () => {
  test("hostile_text_rendered_as_text", () => {
    const { doc, root, ui } = setup();
    // Through the store, as the app does: capped at 500 characters on input.
    const { state } = applyEdit(emptyState(), { kind: "add", id: ID(1), text: HOSTILE, created: 1 });
    const text = state.todos[0].text;
    assert.equal([...text].length, MAX_TEXT);
    assert.ok(text.startsWith("<img src=x"));

    ui.showList({ todos: state.todos, status: "saved", ready: true }, listHandlers());
    const items = root.all("LI");
    assert.equal(items.length, 1);
    const li = items[0];
    assert.equal(li.textContent.includes(text), true, "the text is shown verbatim");
    // Exactly the elements the view builds, nothing parsed out of the text.
    const tags = [...li.walk()].map((e) => e.tagName);
    assert.deepEqual(tags, ["LI", "LABEL", "INPUT", "SPAN", "BUTTON"]);
    const span = li.all("SPAN")[0];
    assert.equal(span.textContent, text);
    assert.equal(span.childNodes.length, 1);
    assert.equal(span.childNodes[0].nodeType, 3, "a single text node");
    for (const el of root.walk()) {
      assert.ok(!["SCRIPT", "IMG", "IFRAME"].includes(el.tagName), `unexpected <${el.tagName}>`);
      for (const name of el.attributes.keys()) assert.ok(!/^on/i.test(name), `attribute ${name}`);
    }
    // Rendering the raw hostile string (no cap) is just as inert.
    const ul = renderTodoList(doc, [{ id: ID(2), text: HOSTILE, done: false, created: 1 }], listHandlers());
    assert.equal(ul.all("SPAN")[0].textContent, HOSTILE);
    assert.equal(ul.all("SCRIPT").length + ul.all("IMG").length, 0);
  });

  test("the input is limited to 500 characters and add passes the text on", () => {
    const { root, ui } = setup();
    const log = [];
    ui.showList({ todos: [], status: "saved", ready: true }, listHandlers(log));
    const input = root.all("INPUT").find((e) => e.type === "text");
    assert.equal(input.getAttribute("maxlength"), String(MAX_TEXT));
    input.value = "  buy milk ";
    const form = root.all("FORM")[0];
    assert.equal(form.fire("submit").prevented, true, "no navigation (form-action 'none')");
    assert.deepEqual(log, [["add", "  buy milk "]]);
    assert.equal(input.value, "", "cleared after add");
    // Empty input adds nothing.
    form.fire("submit");
    assert.equal(log.length, 1);
  });

  test("the list ticks, deletes and locks, and updates in place", () => {
    const { root, ui } = setup();
    const log = [];
    const todos = [
      { id: ID(1), text: "a", done: false, created: 1 },
      { id: ID(2), text: "b", done: true, created: 2 },
    ];
    ui.showList({ todos, status: "loading", ready: false }, listHandlers(log));
    const input = root.all("INPUT").find((e) => e.type === "text");
    assert.equal(input.disabled, true, "no edits before the list is loaded");
    assert.match(root.textContent, /loading/);
    input.value = "typed";
    ui.showList({ todos, status: "saving", ready: true }, listHandlers(log));
    const input2 = root.all("INPUT").find((e) => e.type === "text");
    assert.equal(input2, input, "the input survives an update");
    assert.equal(input2.value, "typed");
    assert.equal(input2.disabled, false);
    assert.match(root.textContent, /saving…/);
    const boxes = root.all("INPUT").filter((e) => e.type === "checkbox");
    assert.deepEqual(
      boxes.map((b) => b.checked),
      [false, true],
    );
    boxes[1].fire("change");
    root.all("LI")[0].button("Delete").fire("click");
    root.button("Lock").fire("click");
    assert.deepEqual(log, [["toggle", ID(2)], ["delete", ID(1)], ["lock"]]);
    ui.showList({ todos: [], status: "saved", ready: true }, listHandlers(log));
    assert.equal(root.all("LI").length, 0);
    assert.match(root.textContent, /saved/);
  });

  test("the code screen has the code, Codes match and Cancel", () => {
    const { root, ui } = setup();
    const log = [];
    ui.showSas("123456", { onMatch: () => log.push("match"), onCancel: () => log.push("cancel") });
    assert.match(root.textContent, /123456/);
    assert.match(root.textContent, /Compare it with the code on your phone/);
    root.button("Codes match").fire("click");
    root.button("Cancel").fire("click");
    assert.deepEqual(log, ["match", "cancel"]);
  });

  test("the offer shows the QR, not the text; the copy button only in dev builds", () => {
    const offer = { text: "vault-pair:v1:SECRET", qr: { size: 2, modules: new Uint8Array([1, 0, 0, 1]) } };
    const prod = setup({ dev: false });
    const log = [];
    prod.ui.showOffer(offer, { onCancel: () => log.push("cancel"), onCopy: () => log.push("copy") });
    assert.match(prod.root.textContent, /Scan with your vault/);
    assert.ok(!prod.root.textContent.includes("SECRET"), "the pairing text is not shown");
    assert.equal(prod.root.all("svg").length, 1);
    assert.ok(!prod.root.all("BUTTON").some((b) => /copy/i.test(b.textContent)), "no copy button");
    prod.root.button("Cancel").fire("click");

    const dev = setup({ dev: true });
    dev.ui.showOffer(offer, { onCancel: noop, onCopy: () => log.push("copy") });
    const copy = dev.root.button("Copy pairing text");
    copy.fire("click");
    assert.match(dev.root.textContent, /Development build/);
    assert.ok(!dev.root.textContent.includes("SECRET"));
    assert.deepEqual(log, ["cancel", "copy"]);
  });

  test("a missing QR says so", () => {
    const { root, ui } = setup();
    ui.showOffer({ text: "x", qr: null }, { onCancel: noop });
    assert.match(root.textContent, /QR code unavailable/);
  });

  test("closed shows the notice as text and offers to pair again", () => {
    const { root, ui } = setup();
    let again = 0;
    ui.showClosed("This vault belongs to a different app or address.", { onPairAgain: () => again++ });
    assert.match(root.textContent, /different app/);
    root.button("Pair again").fire("click");
    assert.equal(again, 1);
    ui.showClosed(HOSTILE, { onPairAgain: noop });
    assert.equal(root.all("SCRIPT").length + root.all("IMG").length, 0);
  });

  test("start screen connects", () => {
    const { root, ui } = setup();
    let n = 0;
    ui.showStart({ onConnect: () => n++ });
    root.button("Connect to vault").fire("click");
    assert.equal(n, 1);
  });
});

describe("renderQr", () => {
  test("one square per dark module, with a 4-module quiet zone", () => {
    const doc = new ShimDocument();
    // 3x3: dark at (0,0), (2,0), (1,1), (2,2).
    const svg = renderQr(doc, { size: 3, modules: new Uint8Array([1, 0, 1, 0, 1, 0, 0, 0, 1]) });
    assert.equal(svg.namespaceURI, SVG);
    assert.equal(svg.tagName, "svg");
    assert.equal(svg.getAttribute("viewBox"), "0 0 11 11");
    const paths = svg.all("path");
    assert.equal(paths.length, 1);
    assert.equal(paths[0].getAttribute("d"), "M4 4h1v1h-1zM6 4h1v1h-1zM5 5h1v1h-1zM6 6h1v1h-1z");
    assert.ok(svg.all("rect").length === 1, "a light background");
  });

  test("refuses a matrix that does not match its size", () => {
    const doc = new ShimDocument();
    assert.throws(() => renderQr(doc, { size: 3, modules: new Uint8Array(8) }), /QR/);
  });
});
