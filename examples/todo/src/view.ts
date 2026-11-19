// SPDX-License-Identifier: MIT
/**
 * The screens. Every node is made with `createElement`/`createElementNS` and
 * every string goes in through `textContent` (or a plain attribute), never
 * through an HTML parser: to-do text is untrusted. The document is passed in so
 * the unit tests can run this on a tiny DOM shim.
 */

import { MAX_TEXT, type Todo } from "./store.js";

/** The QR code as vault-web delivers it: `size * size` modules row by row, 1 = dark. */
export interface QrMatrix {
  size: number;
  modules: Uint8Array;
}

export interface Offer {
  text: string;
  qr: QrMatrix | null;
}

export type SaveStatus = "loading" | "saving" | "saved" | "error";

export interface ListModel {
  todos: readonly Todo[];
  status: SaveStatus;
  /** False until the list is loaded: no edits before that. */
  ready: boolean;
}

export interface ListHandlers {
  onAdd(text: string): void;
  onToggle(id: string): void;
  onDelete(id: string): void;
  onLock(): void;
}

export interface Ui {
  showStart(h: { onConnect(): void }): void;
  /** "Scan with your vault" and the QR; `onCopy` is used only in `--dev` builds. */
  showOffer(offer: Offer, h: { onCancel(): void; onCopy?: () => void }): void;
  /** The code with "Codes match" and "Cancel". */
  showSas(sas: string, h: { onMatch(): void; onCancel(): void }): void;
  /** The list; called again on every change, it updates in place. */
  showList(model: ListModel, h: ListHandlers): void;
  showClosed(message: string, h: { onPairAgain(): void }): void;
}

const SVG_NS = "http://www.w3.org/2000/svg";
const QUIET = 4;

const STATUS_TEXT: Record<SaveStatus, string> = {
  loading: "loading…",
  saving: "saving…",
  saved: "saved",
  error: "not saved",
};

function el<K extends keyof HTMLElementTagNameMap>(
  doc: Document,
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = doc.createElement(tag);
  if (className !== undefined) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

function button(doc: Document, text: string, onClick: () => void, className?: string): HTMLButtonElement {
  const b = el(doc, "button", className, text);
  b.type = "button";
  b.addEventListener("click", () => onClick());
  return b;
}

/** The QR code as an SVG (one path of 1x1 squares, light quiet zone of 4 modules). */
export function renderQr(doc: Document, qr: QrMatrix): SVGSVGElement {
  const { size, modules } = qr;
  if (!Number.isInteger(size) || size < 1 || modules.length !== size * size) {
    throw new RangeError(`QR matrix of size ${size} with ${modules.length} modules`);
  }
  const total = size + 2 * QUIET;
  const svg = doc.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${total} ${total}`);
  svg.setAttribute("class", "qr");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Pairing QR code");
  svg.setAttribute("shape-rendering", "crispEdges");
  const bg = doc.createElementNS(SVG_NS, "rect");
  bg.setAttribute("width", String(total));
  bg.setAttribute("height", String(total));
  bg.setAttribute("fill", "#fff");
  let d = "";
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      if (modules[y * size + x] === 1) d += `M${x + QUIET} ${y + QUIET}h1v1h-1z`;
    }
  }
  const path = doc.createElementNS(SVG_NS, "path");
  path.setAttribute("d", d);
  path.setAttribute("fill", "#000");
  svg.append(bg, path);
  return svg;
}

/** The to-do items: a checkbox and the text (as text) in a label, and a delete button. */
export function renderTodoList(doc: Document, todos: readonly Todo[], h: ListHandlers): HTMLUListElement {
  const ul = el(doc, "ul", "todos");
  for (const t of todos) {
    const li = el(doc, "li", t.done ? "todo done" : "todo");
    const label = el(doc, "label");
    const box = el(doc, "input");
    box.type = "checkbox";
    box.checked = t.done;
    box.addEventListener("change", () => h.onToggle(t.id));
    const span = el(doc, "span", "text", t.text);
    label.append(box, span);
    const del = button(doc, "Delete", () => h.onDelete(t.id), "delete");
    li.append(label, del);
    ul.append(li);
  }
  return ul;
}

interface ListRefs {
  input: HTMLInputElement;
  add: HTMLButtonElement;
  status: HTMLElement;
  list: HTMLUListElement;
  /** The handlers the current render uses (the form's listener reads them). */
  h: ListHandlers;
}

export function createUi(doc: Document, root: HTMLElement, opts: { dev: boolean }): Ui {
  let list: ListRefs | undefined;

  function screen(...children: Node[]): void {
    list = undefined;
    root.replaceChildren(...children);
  }

  function buildList(h: ListHandlers): ListRefs {
    const form = el(doc, "form", "add");
    const input = el(doc, "input");
    input.type = "text";
    input.setAttribute("maxlength", String(MAX_TEXT));
    input.setAttribute("aria-label", "New to-do");
    input.placeholder = "New to-do";
    input.autocomplete = "off";
    const add = el(doc, "button", undefined, "Add");
    add.type = "submit";
    form.append(input, add);
    const status = el(doc, "p", "status");
    status.setAttribute("aria-live", "polite");
    const listEl = el(doc, "ul", "todos");
    const lock = button(doc, "Lock", () => refs.h.onLock(), "lock");
    const refs: ListRefs = { input, add, status, list: listEl, h };
    form.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const text = input.value;
      if (text.trim() === "" || input.disabled) return;
      input.value = "";
      refs.h.onAdd(text);
    });
    root.replaceChildren(el(doc, "h1", undefined, "To-do"), form, status, listEl, lock);
    return refs;
  }

  return {
    showStart(h) {
      screen(
        el(doc, "h1", undefined, "Vault to-do"),
        el(doc, "p", undefined, "Your to-dos are stored only in your vault. Nothing is kept in this browser."),
        button(doc, "Connect to vault", () => h.onConnect(), "primary"),
      );
    },

    showOffer(offer, h) {
      const parts: Node[] = [el(doc, "h1", undefined, "Connect"), el(doc, "p", "lead", "Scan with your vault")];
      parts.push(offer.qr ? renderQr(doc, offer.qr) : el(doc, "p", "notice", "QR code unavailable."));
      parts.push(el(doc, "p", "hint", "The code expires after 60 seconds."));
      // `__DEV__` is false in release builds, so esbuild drops this branch and its strings.
      if (__DEV__ && opts.dev && h.onCopy) {
        const onCopy = h.onCopy;
        parts.push(
          el(
            doc,
            "p",
            "warning",
            "Development build: the pairing text lets whoever has it ask your vault for a session. " +
              "Copy it only into your own vault-dev, never into a chat or a shared terminal.",
          ),
          button(doc, "Copy pairing text", () => onCopy(), "dev-copy"),
        );
      }
      parts.push(button(doc, "Cancel", () => h.onCancel()));
      screen(...parts);
    },

    showSas(sas, h) {
      screen(
        el(doc, "h1", undefined, "Compare the code"),
        el(doc, "p", "sas", sas),
        el(doc, "p", "lead", "Compare it with the code on your phone"),
        button(doc, "Codes match", () => h.onMatch(), "primary"),
        button(doc, "Cancel", () => h.onCancel()),
      );
    },

    showList(model, h) {
      const refs = list ?? buildList(h);
      list = refs;
      refs.h = h;
      refs.input.disabled = !model.ready;
      refs.add.disabled = !model.ready;
      refs.status.textContent = STATUS_TEXT[model.status];
      const fresh = renderTodoList(doc, model.todos, h);
      refs.list.replaceWith(fresh);
      refs.list = fresh;
    },

    showClosed(message, h) {
      const notice = el(doc, "p", "notice", message);
      notice.setAttribute("role", "status");
      screen(el(doc, "h1", undefined, "Disconnected"), notice, button(doc, "Pair again", () => h.onPairAgain(), "primary"));
    },
  };
}
