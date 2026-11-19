// SPDX-License-Identifier: MIT
/**
 * The to-do state, without I/O: parse what the vault holds, apply an edit and
 * say which writes it needs, and debounce commits. Everything here runs in Node
 * for the unit tests.
 *
 * Stored format (one entry per to-do):
 *   key   `todo/<32 lower-case hex>` (16 random bytes)
 *   value UTF-8 JSON `{"text": string (1..500 characters), "done": boolean, "created": number}`
 */

/** The app id the vault is created for (`vaultctl init --app vault-todo`). */
export const APP_ID = "vault-todo";
/** Every to-do key starts with this; the app lists only this prefix. */
export const PREFIX = "todo/";
/** At most this many characters (Unicode code points) of text are kept. */
export const MAX_TEXT = 500;
/** Commits run after this much quiet. */
export const DEBOUNCE_MS = 500;

export interface Todo {
  /** 32 lower-case hex digits. */
  id: string;
  text: string;
  done: boolean;
  /** Milliseconds since the epoch; the list is sorted by it. */
  created: number;
}

export interface State {
  /** Sorted by `created`, then `id`. */
  readonly todos: readonly Todo[];
}

export type Edit =
  | { kind: "add"; id: string; text: string; created: number }
  | { kind: "toggle"; id: string }
  | { kind: "delete"; id: string };

export type Write = { op: "put"; key: string; value: string } | { op: "delete"; key: string };

export interface StoredEntry {
  key: string;
  value: Uint8Array;
}

const ID_RE = /^[0-9a-f]{32}$/;

export function isTodoId(id: unknown): id is string {
  return typeof id === "string" && ID_RE.test(id);
}

export function keyOf(id: string): string {
  return `${PREFIX}${id}`;
}

/** A new id: 16 bytes from `random` (default: the platform CSPRNG) as hex. */
export function newId(random: () => Uint8Array = () => crypto.getRandomValues(new Uint8Array(16))): string {
  const bytes = random();
  if (bytes.length !== 16) throw new RangeError("newId: need 16 random bytes");
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

export function emptyState(): State {
  return { todos: [] };
}

/** `text` trimmed and cut to `MAX_TEXT` characters (never inside a surrogate pair). */
export function capText(text: string): string {
  const trimmed = text.trim();
  if (trimmed.length <= MAX_TEXT) return trimmed;
  return Array.from(trimmed).slice(0, MAX_TEXT).join("");
}

function charCount(s: string): number {
  let n = 0;
  for (const _ of s) n++;
  return n;
}

function compareTodos(a: Todo, b: Todo): number {
  if (a.created !== b.created) return a.created - b.created;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

function sorted(todos: Todo[]): Todo[] {
  return todos.sort(compareTodos);
}

function encodeTodo(t: Todo): string {
  return JSON.stringify({ text: t.text, done: t.done, created: t.created });
}

const strictUtf8 = new TextDecoder("utf-8", { fatal: true });

/** The to-do in one stored entry, or why it is not one. */
function parseEntry(e: StoredEntry): Todo | string {
  if (typeof e.key !== "string" || !e.key.startsWith(PREFIX)) return "not a to-do key";
  const id = e.key.slice(PREFIX.length);
  if (!isTodoId(id)) return "bad id";
  let v: unknown;
  try {
    v = JSON.parse(strictUtf8.decode(e.value));
  } catch {
    return "not UTF-8 JSON";
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return "not an object";
  const o = v as Record<string, unknown>;
  const { text, done, created } = o;
  if (typeof text !== "string" || text.trim() === "" || charCount(text) > MAX_TEXT) return "bad text";
  if (typeof done !== "boolean") return "bad done";
  if (typeof created !== "number" || !Number.isFinite(created)) return "bad created";
  return { id, text, done, created };
}

/**
 * The to-dos in `entries` (from `list("todo/")`), sorted. Entries that are not
 * valid to-dos are skipped, each with a warning naming the key (never the value).
 */
export function parseEntries(
  entries: readonly StoredEntry[],
  warn: (...args: unknown[]) => void = console.warn,
): { todos: Todo[]; skipped: number } {
  const todos: Todo[] = [];
  let skipped = 0;
  for (const e of entries) {
    const r = parseEntry(e);
    if (typeof r === "string") {
      skipped++;
      warn(`vault-todo: skipped ${JSON.stringify(String(e.key))}: ${r}`);
    } else {
      todos.push(r);
    }
  }
  return { todos: sorted(todos), skipped };
}

/** The state after `edit`, and the writes that make it durable. Never mutates `state`. */
export function applyEdit(state: State, edit: Edit): { state: State; writes: Write[] } {
  const none = { state, writes: [] };
  switch (edit.kind) {
    case "add": {
      const text = capText(edit.text);
      if (text === "" || !isTodoId(edit.id) || !Number.isFinite(edit.created)) return none;
      if (state.todos.some((t) => t.id === edit.id)) return none;
      const todo: Todo = { id: edit.id, text, done: false, created: edit.created };
      return {
        state: { todos: sorted([...state.todos, todo]) },
        writes: [{ op: "put", key: keyOf(todo.id), value: encodeTodo(todo) }],
      };
    }
    case "toggle": {
      const old = state.todos.find((t) => t.id === edit.id);
      if (!old) return none;
      const todo: Todo = { ...old, done: !old.done };
      return {
        state: { todos: state.todos.map((t) => (t.id === edit.id ? todo : t)) },
        writes: [{ op: "put", key: keyOf(todo.id), value: encodeTodo(todo) }],
      };
    }
    case "delete": {
      if (!state.todos.some((t) => t.id === edit.id)) return none;
      return {
        state: { todos: state.todos.filter((t) => t.id !== edit.id) },
        writes: [{ op: "delete", key: keyOf(edit.id) }],
      };
    }
  }
}

/** Only the last write to each key, in the order of those last writes. */
export function coalesceWrites(writes: readonly Write[]): Write[] {
  const last = new Map<string, Write>();
  for (const w of writes) {
    last.delete(w.key);
    last.set(w.key, w);
  }
  return [...last.values()];
}

/**
 * Collects items and hands them to `flush` together once `ms` passed without a
 * new one. Flushes never overlap: one that comes due while another runs waits
 * for it. `drop()` forgets what has not been flushed yet.
 */
export class Debouncer<T> {
  readonly #ms: number;
  readonly #flush: (items: T[]) => void | Promise<void>;
  readonly #onError: (e: unknown) => void;
  #items: T[] = [];
  #timer: ReturnType<typeof setTimeout> | undefined;
  #running = 0;
  #chain: Promise<void> = Promise.resolve();

  constructor(
    ms: number,
    flush: (items: T[]) => void | Promise<void>,
    onError: (e: unknown) => void = (e) => console.warn("vault-todo: flush failed:", e),
  ) {
    if (!Number.isFinite(ms) || ms < 0) throw new RangeError("Debouncer: ms must be >= 0");
    this.#ms = ms;
    this.#flush = flush;
    this.#onError = onError;
  }

  /** Items waiting for the quiet period. */
  get pending(): number {
    return this.#items.length;
  }

  /** Batches handed to `flush` that have not finished (the running one and those queued behind it). */
  get running(): number {
    return this.#running;
  }

  /** Something is waiting or being flushed. */
  get busy(): boolean {
    return this.#items.length > 0 || this.#running > 0;
  }

  push(item: T): void {
    this.#items.push(item);
    if (this.#timer !== undefined) clearTimeout(this.#timer);
    this.#timer = setTimeout(() => this.#fire(), this.#ms);
  }

  /** Forget the pending items (a flush already running is not stopped). Returns how many. */
  drop(): number {
    if (this.#timer !== undefined) clearTimeout(this.#timer);
    this.#timer = undefined;
    const n = this.#items.length;
    this.#items = [];
    return n;
  }

  /** Resolves once nothing is pending and no flush runs (waiting out the quiet period). */
  async idle(): Promise<void> {
    while (this.busy) {
      if (this.#running > 0) {
        await this.#chain;
      } else {
        await new Promise((r) => setTimeout(r, this.#ms));
      }
    }
  }

  #fire(): void {
    this.#timer = undefined;
    const items = this.#items;
    this.#items = [];
    if (items.length === 0) return;
    this.#running++;
    this.#chain = this.#chain
      .then(() => this.#flush(items))
      .catch((e: unknown) => this.#onError(e))
      .finally(() => {
        this.#running--;
      });
  }
}

/** The notice for a closed session (`docs/integration.md`, "Handling Closed(reason)"). */
export function closeMessage(reason: string | undefined, detail = ""): string {
  switch (reason) {
    case "denied":
      return "Pairing was cancelled.";
    case "wrongApp":
      return "This vault belongs to a different app or address.";
    case "locked":
      return "Vault locked.";
    case "terminated":
      return `The vault ended the session (${detail || "terminated"}).`;
    case "disconnected":
      return "Connection to the vault was lost.";
    case "commitFailed":
      return "Your last change was not saved.";
    case "relayUnreachable":
      return `Could not reach the relay (${detail || "relay unreachable"}).`;
    case "pairingFailed":
      return `Pairing failed (${detail || "pairing failed"}). Pair again with a new code.`;
    case "keepaliveFailed":
    case "keepaliveTimedOut":
      return "The vault stopped answering.";
    case "sendFailed":
    case "protocolError":
    case "closed":
      return `The connection failed (${detail || reason}). Pair again.`;
    case "failed":
      return `The session failed (${detail || "failed"}).`;
    default:
      // "cancelled" (lock before the handshake finished): nothing to report.
      return "Disconnected.";
  }
}
