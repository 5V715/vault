// SPDX-License-Identifier: MIT
/**
 * vault-web: pair a web app with the user's vault and use it as the app's only
 * store. Typed wrapper over the wasm-bindgen module built from `vault-web/`
 * (`npm run build:wasm`). Read `docs/integration.md` first: no data call before
 * `active`, the code is shown with "Codes match" and "Cancel", and `closed`
 * discards every uncommitted write.
 *
 * Loading the wasm module:
 * - Node 22+: `import { pair } from "vault-web/node"` (loads `pkg/node`).
 * - Browsers: `import * as glue from "vault-web/web"` (the `--target web`
 *   glue), then `await loadWeb(glue, wasmUrl)` once, then `pair(...)` from
 *   `"vault-web"`. `wasmUrl` is where the page serves
 *   `"vault-web/web/vault_web_bg.wasm"` (with esbuild:
 *   `import wasmUrl from "vault-web/web/vault_web_bg.wasm"` and
 *   `--loader:.wasm=file`). Without it the glue fetches `vault_web_bg.wasm`
 *   next to its own URL, which a bundle does not have.
 *
 * Copies of your data in JS: `get` and `list` return fresh `Uint8Array`s and
 * key strings, which this package cannot wipe; the Rust side wipes its own
 * plaintext copies. Keep a reference to the `Session`: if it is garbage
 * collected, the session is locked (`closed`, `locked`).
 *
 * Transport: the runtime's global `WebSocket` (browsers; Node 22+ has one).
 */

/** Session states, in order; `closed` is final. */
export type SessionStateName = "pairing" | "awaitingApproval" | "active" | "closed";

/** Why a session closed: vault-client's `CloseReason` names in camelCase. */
export type CloseReasonName =
  | "denied"
  | "locked"
  | "terminated"
  | "disconnected"
  | "cancelled"
  | "commitFailed"
  | "sendFailed"
  | "protocolError"
  | "closed"
  | "relayUnreachable"
  | "pairingFailed"
  | "keepaliveFailed"
  | "keepaliveTimedOut"
  | "failed"
  | "wrongApp";

export const SESSION_STATES: readonly SessionStateName[] = [
  "pairing",
  "awaitingApproval",
  "active",
  "closed",
];

export const CLOSE_REASONS: readonly CloseReasonName[] = [
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

/** A QR code without quiet zone: `size * size` modules row by row, 1 = dark. */
export interface QrMatrix {
  size: number;
  modules: Uint8Array;
}

export interface PairingOffer {
  /** The pairing text (`vault-pair:v1:...`). Secret-ish: show it only as the QR. */
  text: string;
  /** Its QR code (level M); `null` only if the text does not fit one. */
  qr: QrMatrix | null;
}

export interface PairOptions {
  /** The relay's origin, e.g. `https://relay.example` (`http` only for loopback). */
  relayOrigin: string;
  /** The app id the vault was created for (`^[a-z0-9][a-z0-9.-]{0,63}$`). */
  appId: string;
  /** Shown on the vault's approval prompt (at most 64 bytes, no control characters). */
  deviceName: string;
  /** Show `offer.qr` (and nothing else of the text) with "Scan with your vault". */
  onOffer(offer: PairingOffer): void;
  /** Show the 6-digit code with "Codes match" (`confirmSas`) and "Cancel" (`cancel`). */
  onSas(sas: string): void;
  /**
   * Drive the UI. When `state` is `closed`, `reason` says why and `detail` is
   * its text (with any detail, e.g. `relay unreachable: ...`).
   */
  onState(state: SessionStateName, reason?: CloseReasonName, detail?: string): void;
  /**
   * TEST ONLY, for runtimes without `location` (Node). In a browser the page
   * origin is always `location.origin`; a different value here is refused.
   */
  testOrigin?: string;
}

export interface Entry {
  key: string;
  value: Uint8Array;
}

export interface Session {
  /** "Codes match": `active` once the vault approved too. */
  confirmSas(): Promise<void>;
  /** "Cancel" before `active`: closes as `denied`. Rejects once active or closed. */
  cancel(): Promise<void>;
  /** End the session; resolves once `closed`. */
  lock(): Promise<void>;
  state(): SessionStateName;
  /** Data calls reject unless `active`, sending nothing. */
  get(key: string): Promise<Uint8Array | undefined>;
  /** Buffered until `commit()`. */
  put(key: string, value: Uint8Array): Promise<void>;
  /** Buffered until `commit()`. */
  delete(key: string): Promise<void>;
  /** Entries whose key starts with `prefix`, in key order (buffered writes included). */
  list(prefix: string): Promise<Entry[]>;
  /** Make every earlier write durable. A failure closes the session (`commitFailed`). */
  commit(): Promise<void>;
}

/** The raw options the wasm `pair` takes. */
export interface RawPairOptions {
  relayOrigin: string;
  appId: string;
  deviceName: string;
  testOrigin?: string;
  onOffer(offer: PairingOffer): void;
  onSas(sas: string): void;
  onState(state: string, reason?: string, detail?: string): void;
}

/** The wasm-bindgen `Session` class (`confirmSas` and `cancel` throw synchronously). */
export interface RawSession {
  confirmSas(): void | Promise<unknown>;
  cancel(): void | Promise<unknown>;
  lock(): Promise<unknown>;
  state(): string;
  get(key: string): Promise<Uint8Array | undefined>;
  put(key: string, value: Uint8Array): Promise<unknown>;
  delete(key: string): Promise<unknown>;
  list(prefix: string): Promise<Entry[]>;
  commit(): Promise<unknown>;
}

/** What this wrapper needs from the wasm-bindgen module (either target). */
export interface WasmBindings {
  pair(opts: RawPairOptions): RawSession;
  qrMatrix(text: string): QrMatrix;
}

/** The `--target web` module: its default export instantiates the wasm. */
export interface WebGlue extends WasmBindings {
  default(init?: { module_or_path: WasmSource }): Promise<unknown>;
}

/** Where `loadWeb` gets the wasm: a URL to fetch, its bytes, or a compiled module. */
export type WasmSource = string | URL | BufferSource | WebAssembly.Module;

/** The vault's limit on key and prefix length (`vault_proto::msg::MAX_KEY`). */
export const MAX_KEY_BYTES = 1024;

let wasm: WasmBindings | undefined;

/** Use these bindings (the loaders call it; tests pass a fake). */
export function setWasm(bindings: WasmBindings): void {
  if (
    typeof bindings !== "object" ||
    bindings === null ||
    typeof bindings.pair !== "function" ||
    typeof bindings.qrMatrix !== "function"
  ) {
    throw new TypeError("vault-web: not a vault-web wasm module (pair/qrMatrix missing)");
  }
  wasm = bindings;
}

/**
 * Instantiate the `--target web` glue (`import * as glue from "vault-web/web"`)
 * from `source` (default: `vault_web_bg.wasm` next to the glue) and use it.
 */
export async function loadWeb(glue: WebGlue, source?: WasmSource): Promise<void> {
  await glue.default(source === undefined ? undefined : { module_or_path: source });
  setWasm(glue);
}

function bindings(): WasmBindings {
  if (!wasm) {
    throw new Error(
      'vault-web: wasm not loaded (import "vault-web/node", or call loadWeb() in a browser)',
    );
  }
  return wasm;
}

/** The QR matrix of `text`, as `onOffer` gets it. */
export function qrMatrix(text: string): QrMatrix {
  if (typeof text !== "string") throw new TypeError("qrMatrix: text must be a string");
  return bindings().qrMatrix(text);
}

export function toStateName(s: unknown): SessionStateName {
  if ((SESSION_STATES as readonly unknown[]).includes(s)) return s as SessionStateName;
  throw new Error(`vault-web: unknown session state ${JSON.stringify(s)}`);
}

export function toCloseReasonName(r: unknown): CloseReasonName {
  if ((CLOSE_REASONS as readonly unknown[]).includes(r)) return r as CloseReasonName;
  throw new Error(`vault-web: unknown close reason ${JSON.stringify(r)}`);
}

function requireString(opts: Record<string, unknown>, name: string): string {
  const v = opts[name];
  if (typeof v !== "string" || v === "") {
    throw new TypeError(`pair: ${name} must be a non-empty string`);
  }
  return v;
}

function requireFunction<F>(opts: Record<string, unknown>, name: string): F {
  const v = opts[name];
  if (typeof v !== "function") throw new TypeError(`pair: ${name} must be a function`);
  return v as F;
}

const utf8 = new TextEncoder();

function checkKey(key: unknown, what: "key" | "prefix"): string {
  if (typeof key !== "string") throw new TypeError(`${what} must be a string`);
  if (what === "key" && key === "") throw new TypeError("key must not be empty");
  if (utf8.encode(key).length > MAX_KEY_BYTES) {
    throw new RangeError(`${what} longer than ${MAX_KEY_BYTES} bytes`);
  }
  return key;
}

class WrappedSession implements Session {
  readonly #raw: RawSession;

  constructor(raw: RawSession) {
    this.#raw = raw;
  }

  async confirmSas(): Promise<void> {
    await this.#raw.confirmSas();
  }

  async cancel(): Promise<void> {
    await this.#raw.cancel();
  }

  async lock(): Promise<void> {
    await this.#raw.lock();
  }

  state(): SessionStateName {
    return toStateName(this.#raw.state());
  }

  async get(key: string): Promise<Uint8Array | undefined> {
    return this.#raw.get(checkKey(key, "key"));
  }

  async put(key: string, value: Uint8Array): Promise<void> {
    const k = checkKey(key, "key");
    if (!(value instanceof Uint8Array)) throw new TypeError("value must be a Uint8Array");
    await this.#raw.put(k, value);
  }

  async delete(key: string): Promise<void> {
    await this.#raw.delete(checkKey(key, "key"));
  }

  async list(prefix: string): Promise<Entry[]> {
    return this.#raw.list(checkKey(prefix, "prefix"));
  }

  async commit(): Promise<void> {
    await this.#raw.commit();
  }
}

/**
 * Start one pairing. Resolves with the session at once (state `pairing`); the
 * callbacks report the offer, the code and every state change. Pair again with
 * a new call after `closed`. Keep a reference to the session while it is in use.
 */
export async function pair(opts: PairOptions): Promise<Session> {
  if (typeof opts !== "object" || opts === null) {
    throw new TypeError("pair: options object required");
  }
  const o = opts as unknown as Record<string, unknown>;
  const relayOrigin = requireString(o, "relayOrigin");
  const appId = requireString(o, "appId");
  const deviceName = requireString(o, "deviceName");
  const onOffer = requireFunction<PairOptions["onOffer"]>(o, "onOffer");
  const onSas = requireFunction<PairOptions["onSas"]>(o, "onSas");
  const onState = requireFunction<PairOptions["onState"]>(o, "onState");
  const testOrigin = o["testOrigin"];
  if (testOrigin !== undefined && typeof testOrigin !== "string") {
    throw new TypeError("pair: testOrigin must be a string");
  }
  const raw: RawPairOptions = {
    relayOrigin,
    appId,
    deviceName,
    onOffer: (offer) => onOffer(offer),
    onSas: (sas) => onSas(sas),
    onState: (state, reason, detail) => {
      const s = toStateName(state);
      if (s === "closed") {
        onState(s, toCloseReasonName(reason), detail ?? "");
      } else {
        onState(s);
      }
    },
  };
  if (testOrigin !== undefined) raw.testOrigin = testOrigin;
  return new WrappedSession(bindings().pair(raw));
}
