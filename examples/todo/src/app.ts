// SPDX-License-Identifier: MIT
/**
 * The app's wiring, without the DOM or the wasm: `TodoApp` drives the screens
 * from the vault-web session (docs/integration.md, "What the UI must show"),
 * and `TodoList` is the list bound to one Active session. Both take their
 * collaborators as arguments, so the unit tests use fakes and the e2e test uses
 * a real session.
 *
 * The rules this follows:
 * - no data call before `active`: the `TodoList`, the only thing that touches
 *   the store, is created on `active`, and its first call is `list("todo/")`;
 * - "Codes match" and "Cancel" are the user's clicks, never automatic;
 * - `closed` drops the edits still waiting for the debounce and clears the list.
 */

import {
  APP_ID,
  DEBOUNCE_MS,
  Debouncer,
  PREFIX,
  applyEdit,
  closeMessage,
  coalesceWrites,
  emptyState,
  newId,
  parseEntries,
  type Edit,
  type State,
  type Todo,
  type Write,
} from "./store.js";
import type { ListModel, Offer, SaveStatus, Ui } from "./view.js";

type Warn = (...args: unknown[]) => void;

/** The data half of a vault-web `Session`. */
export interface DataSession {
  put(key: string, value: Uint8Array): Promise<void>;
  delete(key: string): Promise<void>;
  list(prefix: string): Promise<{ key: string; value: Uint8Array }[]>;
  commit(): Promise<void>;
}

/** What the app uses of a vault-web `Session`. */
export interface AppSession extends DataSession {
  confirmSas(): Promise<void>;
  cancel(): Promise<void>;
  lock(): Promise<void>;
}

/** The subset of vault-web's `pair` options the app passes. */
export interface AppPairOptions {
  relayOrigin: string;
  appId: string;
  deviceName: string;
  onOffer(offer: Offer): void;
  onSas(sas: string): void;
  onState(state: string, reason?: string, detail?: string): void;
}

export type PairFn = (opts: AppPairOptions) => Promise<AppSession>;

const utf8 = new TextEncoder();

export interface TodoListOptions {
  debounceMs?: number;
  /** Called after every change of `todos`, `status` or `ready`. */
  onChange?: () => void;
  warn?: Warn;
  newId?: () => string;
  now?: () => number;
}

/** The to-do list of one Active session. Discard it when the session closes (`close`). */
export class TodoList {
  readonly #session: DataSession;
  readonly #debouncer: Debouncer<Write>;
  readonly #onChange: () => void;
  readonly #warn: Warn;
  readonly #newId: () => string;
  readonly #now: () => number;
  #state: State = emptyState();
  #status: SaveStatus = "loading";
  #ready = false;
  #loading = false;
  #closed = false;
  #locking = false;
  /** Writes made but not yet committed (pending, queued or in flight). */
  #unsaved = 0;

  constructor(session: DataSession, opts: TodoListOptions = {}) {
    this.#session = session;
    this.#onChange = opts.onChange ?? (() => {});
    this.#warn = opts.warn ?? console.warn;
    this.#newId = opts.newId ?? (() => newId());
    this.#now = opts.now ?? Date.now;
    this.#debouncer = new Debouncer<Write>(
      opts.debounceMs ?? DEBOUNCE_MS,
      (writes) => this.#flush(writes),
      (e) => {
        if (this.#closed) return;
        this.#status = "error";
        this.#warn("vault-todo: saving failed:", e);
        this.#onChange();
      },
    );
  }

  get todos(): readonly Todo[] {
    return this.#state.todos;
  }

  get status(): SaveStatus {
    return this.#status;
  }

  /** Loaded, not locking and not closed: edits are accepted. */
  get ready(): boolean {
    return this.#ready && !this.#closed && !this.#locking;
  }

  /** Lock was clicked: no more edits. */
  get locking(): boolean {
    return this.#locking;
  }

  /** Changes made but not committed yet. */
  get unsaved(): number {
    return this.#unsaved;
  }

  /**
   * Before a voluntary lock: stop taking edits, commit what is pending now and
   * wait for it, at most `ms`. Resolves with the number of changes still not
   * committed (0 when everything was saved).
   */
  async flushForLock(ms: number): Promise<number> {
    this.#locking = true;
    this.#onChange();
    this.#debouncer.flushNow();
    let timer: ReturnType<typeof setTimeout> | undefined;
    await Promise.race([
      this.#debouncer.idle(),
      new Promise<void>((r) => (timer = setTimeout(r, ms))),
    ]);
    clearTimeout(timer);
    return this.#unsaved;
  }

  model(): ListModel {
    return { todos: this.todos, status: this.#status, ready: this.ready };
  }

  /** The initial load (`list("todo/")`); call once, after Active. */
  async load(): Promise<void> {
    if (this.#loading || this.#closed) return;
    this.#loading = true;
    const entries = await this.#session.list(PREFIX);
    if (this.#closed) return;
    const { todos } = parseEntries(entries, this.#warn);
    this.#state = { todos };
    this.#ready = true;
    this.#status = "saved";
    this.#onChange();
  }

  /** Adds a to-do (text trimmed and capped); its id, or undefined if nothing was added. */
  add(text: string): string | undefined {
    const id = this.#newId();
    return this.#edit({ kind: "add", id, text, created: this.#now() }) ? id : undefined;
  }

  toggle(id: string): void {
    this.#edit({ kind: "toggle", id });
  }

  remove(id: string): void {
    this.#edit({ kind: "delete", id });
  }

  /**
   * The session closed: drop pending edits and clear the list. Returns how many
   * changes were not committed (dropped from the debounce, or in flight).
   */
  close(): number {
    this.#closed = true;
    this.#debouncer.drop();
    this.#state = emptyState();
    this.#ready = false;
    return this.#unsaved;
  }

  /** Resolves once every edit so far was committed (or failed, or dropped). */
  idle(): Promise<void> {
    return this.#debouncer.idle();
  }

  #edit(edit: Edit): boolean {
    if (!this.ready) return false;
    const r = applyEdit(this.#state, edit);
    if (r.writes.length === 0) return false;
    this.#state = r.state;
    for (const w of r.writes) this.#debouncer.push(w);
    this.#unsaved += r.writes.length;
    this.#status = "saving";
    this.#onChange();
    return true;
  }

  async #flush(writes: Write[]): Promise<void> {
    // Stop at the first await after a close: the session is gone (its calls
    // would reject anyway) and nothing of this batch may be committed.
    for (const w of coalesceWrites(writes)) {
      if (this.#closed) return;
      if (w.op === "put") await this.#session.put(w.key, utf8.encode(w.value));
      else await this.#session.delete(w.key);
    }
    if (this.#closed) return;
    await this.#session.commit();
    this.#unsaved -= writes.length;
    if (this.#closed) return;
    // "saved" only when this was the last batch: none waiting for the quiet
    // period and none queued behind this one (running counts this flush).
    // A failed commit throws above and the error handler shows "not saved".
    if (this.#debouncer.pending === 0 && this.#debouncer.running === 1) {
      this.#status = "saved";
      this.#onChange();
    }
  }
}

export interface TodoAppOptions {
  pair: PairFn;
  ui: Ui;
  relayOrigin: string;
  deviceName: string;
  /** `--dev` build: offer "Copy pairing text". */
  dev: boolean;
  copyText?: (text: string) => Promise<void>;
  debounceMs?: number;
  /** How long Lock waits for pending changes to be committed (default 5 s). */
  lockFlushMs?: number;
  warn?: Warn;
}

/** Lock waits at most this long for pending changes to be committed. */
export const LOCK_FLUSH_MS = 5_000;

/** " N change(s) were not saved." for a notice, or "". */
export function unsavedNotice(n: number): string {
  if (n <= 0) return "";
  return n === 1 ? " 1 change was not saved." : ` ${n} changes were not saved.`;
}

export class TodoApp {
  readonly #o: TodoAppOptions;
  readonly #warn: Warn;
  /** Bumped per pairing; callbacks of older sessions are ignored. */
  #gen = 0;
  #open = false;
  #list: TodoList | undefined;

  constructor(opts: TodoAppOptions) {
    this.#o = opts;
    this.#warn = opts.warn ?? console.warn;
  }

  /** The list of the Active session, if any (for tests). */
  get list(): TodoList | undefined {
    return this.#list;
  }

  start(): void {
    this.#o.ui.showStart({ onConnect: () => void this.connect() });
  }

  /** Start a new pairing, unless one is already open. */
  async connect(): Promise<void> {
    if (this.#open) return;
    this.#open = true;
    const gen = ++this.#gen;
    const { ui, dev } = this.#o;
    const current = () => gen === this.#gen;
    // Callbacks may come before `pair` resolves; act on the session through this.
    let resolveSession!: (s: AppSession) => void;
    const session = new Promise<AppSession>((r) => (resolveSession = r));
    const act = (what: string, f: (s: AppSession) => Promise<void>) => {
      void session.then(f).catch((e: unknown) => this.#warn(`vault-todo: ${what} failed:`, e));
    };
    const copyText = this.#o.copyText;
    try {
      const s = await this.#o.pair({
        relayOrigin: this.#o.relayOrigin,
        appId: APP_ID,
        deviceName: this.#o.deviceName,
        onOffer: (offer) => {
          if (!current()) return;
          ui.showOffer(offer, {
            onCancel: () => act("cancel", (x) => x.cancel()),
            ...(dev && copyText ? { onCopy: () => copyText(offer.text) } : {}),
          });
        },
        onSas: (sas) => {
          if (!current()) return;
          ui.showSas(sas, {
            onMatch: () => act("confirm", (x) => x.confirmSas()),
            onCancel: () => act("cancel", (x) => x.cancel()),
          });
        },
        onState: (state, reason, detail) => {
          // After `pair` resolved, in the order reported.
          void session.then((x) => {
            if (current()) this.#onState(x, state, reason, detail);
          });
        },
      });
      resolveSession(s);
    } catch (e) {
      this.#open = false;
      this.#warn("vault-todo: pairing could not start:", e);
      ui.showClosed(`Could not start pairing (${String(e)}).`, { onPairAgain: () => void this.connect() });
    }
  }

  /** Lock: first commit what is pending (bounded wait), then end the session. */
  async #lock(session: AppSession, list: TodoList): Promise<void> {
    if (list.locking) return; // a second click
    try {
      const left = await list.flushForLock(this.#o.lockFlushMs ?? LOCK_FLUSH_MS);
      if (left > 0) this.#warn(`vault-todo: locking with ${left} change(s) not committed`);
    } catch (e) {
      this.#warn("vault-todo: saving before lock failed:", e);
    }
    // The closed notice reports whatever is still not committed.
    await session.lock().catch((e: unknown) => this.#warn("vault-todo: lock failed:", e));
  }

  #onState(session: AppSession, state: string, reason?: string, detail?: string): void {
    const { ui } = this.#o;
    if (state === "active") {
      const list = new TodoList(session, {
        warn: this.#warn,
        onChange: () => {
          if (this.#list === list) render();
        },
        ...(this.#o.debounceMs !== undefined ? { debounceMs: this.#o.debounceMs } : {}),
      });
      const render = () =>
        ui.showList(list.model(), {
          onAdd: (text) => void list.add(text),
          onToggle: (id) => list.toggle(id),
          onDelete: (id) => list.remove(id),
          onLock: () => void this.#lock(session, list),
        });
      this.#list = list;
      render();
      list.load().catch((e: unknown) => this.#warn("vault-todo: loading the list failed:", e));
    } else if (state === "closed") {
      this.#open = false;
      const list = this.#list;
      this.#list = undefined;
      const unsaved = list?.close() ?? 0;
      if (unsaved > 0) this.#warn(`vault-todo: ${unsaved} unsaved change(s) dropped`);
      ui.showClosed(closeMessage(reason, detail) + unsavedNotice(unsaved), {
        onPairAgain: () => void this.connect(),
      });
    }
  }
}
