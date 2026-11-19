// SPDX-License-Identifier: MIT
// A tiny DOM for the view tests: elements with children, attributes, text and
// listeners. Every HTML-parsing entry point throws, so a view that used one fails.

class ShimText {
  constructor(text) {
    this.nodeType = 3;
    this.data = text;
    this.parentNode = null;
  }

  get textContent() {
    return this.data;
  }
}

function forbidden(name) {
  return () => {
    throw new Error(`the view used ${name}`);
  };
}

export class ShimElement {
  constructor(doc, tag, ns) {
    this.ownerDocument = doc;
    this.nodeType = 1;
    this.tagName = ns === "http://www.w3.org/2000/svg" ? tag : tag.toUpperCase();
    this.namespaceURI = ns ?? "http://www.w3.org/1999/xhtml";
    this.childNodes = [];
    this.attributes = new Map();
    this.listeners = new Map();
    this.parentNode = null;
    this.className = "";
    this.value = "";
    this.checked = false;
    this.disabled = false;
    this.type = "";
    this.focused = false;
  }

  get children() {
    return this.childNodes.filter((c) => c.nodeType === 1);
  }

  get textContent() {
    return this.childNodes.map((c) => c.textContent).join("");
  }

  set textContent(v) {
    this.#detachAll();
    if (v !== "" && v !== null && v !== undefined) this.#attach(new ShimText(String(v)));
  }

  set innerHTML(_) {
    forbidden("innerHTML")();
  }

  set outerHTML(_) {
    forbidden("outerHTML")();
  }

  insertAdjacentHTML() {
    forbidden("insertAdjacentHTML")();
  }

  #attach(c) {
    if (typeof c === "string") c = new ShimText(c);
    if (c.parentNode) c.parentNode.removeChild(c);
    c.parentNode = this;
    this.childNodes.push(c);
    return c;
  }

  #detachAll() {
    for (const c of this.childNodes) c.parentNode = null;
    this.childNodes = [];
  }

  appendChild(c) {
    return this.#attach(c);
  }

  append(...cs) {
    for (const c of cs) this.#attach(c);
  }

  replaceChildren(...cs) {
    this.#detachAll();
    this.append(...cs);
  }

  removeChild(c) {
    this.childNodes = this.childNodes.filter((x) => x !== c);
    c.parentNode = null;
    return c;
  }

  replaceWith(n) {
    const p = this.parentNode;
    if (!p) return;
    const i = p.childNodes.indexOf(this);
    if (n.parentNode) n.parentNode.removeChild(n);
    p.childNodes[i] = n;
    n.parentNode = p;
    this.parentNode = null;
  }

  remove() {
    this.parentNode?.removeChild(this);
  }

  setAttribute(name, value) {
    if (/^on/i.test(name)) throw new Error(`the view set an event handler attribute: ${name}`);
    if (name === "style") throw new Error("the view set a style attribute (CSP style-src 'self')");
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(fn);
  }

  /** Test helper: run the `type` listeners. */
  fire(type, extra = {}) {
    let prevented = false;
    const ev = { type, target: this, preventDefault: () => (prevented = true), ...extra };
    for (const fn of this.listeners.get(type) ?? []) fn(ev);
    return { prevented };
  }

  focus() {
    this.focused = true;
  }

  /** Test helper: every element below (and including) this one. */
  *walk() {
    yield this;
    for (const c of this.children) yield* c.walk();
  }

  /** Test helper: the elements with `tag` (HTML tags upper-case). */
  all(tag) {
    return [...this.walk()].filter((e) => e.tagName === tag);
  }

  /** Test helper: the buttons by their text. */
  button(text) {
    const b = this.all("BUTTON").find((e) => e.textContent === text);
    if (!b) throw new Error(`no button "${text}"; buttons: ${this.all("BUTTON").map((e) => e.textContent)}`);
    return b;
  }
}

export class ShimDocument {
  createElement(tag) {
    return new ShimElement(this, tag);
  }

  createElementNS(ns, tag) {
    return new ShimElement(this, tag, ns);
  }

  createTextNode(text) {
    return new ShimText(String(text));
  }

  write() {
    forbidden("document.write")();
  }
}
