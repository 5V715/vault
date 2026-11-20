// SPDX-License-Identifier: MIT
/**
 * The relay origin of image builds (`build.mjs --runtime-config`): vault-relay
 * serves the page and writes `/config.json` (`{"relay":"wss://host[:port]"}`)
 * from its own configuration, and the page reads it at start. The CSP the relay
 * sends allows exactly that relay (and 'self' for this fetch), so a wrong value
 * fails closed; it is still checked strictly here: a canonical `wss:` origin, or
 * `ws:` for a loopback relay from an http page (local testing) only.
 */

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);

/** Same shape the relay writes: scheme, lower-case host or [IPv6], optional port. */
const ORIGIN = /^(wss|ws):\/\/([a-z0-9.-]+|\[[0-9a-f:.]+\])(:[0-9]{1,5})?$/;

/**
 * The relay's origin in the https/http form vault-web's `pair` takes, from the
 * parsed `/config.json`. `pageProtocol` is `location.protocol`.
 */
export function relayFromConfig(config: unknown, pageProtocol: string): string {
  if (typeof config !== "object" || config === null || Array.isArray(config)) {
    throw new Error("bad /config.json: expected an object with a relay");
  }
  const keys = Object.keys(config);
  const relay: unknown = (config as { relay?: unknown }).relay;
  if (keys.length !== 1 || typeof relay !== "string") {
    throw new Error('bad /config.json: expected exactly {"relay": "wss://host[:port]"}');
  }
  const bad = (why: string): never => {
    throw new Error(`bad relay origin ${JSON.stringify(relay)} in /config.json: ${why}`);
  };
  const m = ORIGIN.exec(relay);
  if (!m) bad("expected a canonical wss://host[:port]");
  let u: URL;
  try {
    u = new URL(relay);
  } catch {
    return bad("not a URL");
  }
  // The URL parser must agree with the pattern (no credentials, path or default port).
  if (u.username || u.password || u.search || u.hash || (u.pathname !== "" && u.pathname !== "/")) {
    bad("no credentials, path, query or fragment");
  }
  const asHttp = relay.replace(/^wss:/, "https:").replace(/^ws:/, "http:");
  if (new URL(asHttp).origin !== asHttp) bad("not in canonical form");
  if (u.protocol === "ws:") {
    if (!LOOPBACK.has(u.hostname)) bad("ws: is only allowed for loopback hosts");
    if (pageProtocol !== "http:") bad("an https page cannot use a ws: relay (mixed content)");
  }
  return asHttp;
}

/** What `loadRuntimeRelay` uses of `fetch` (injectable for tests). */
export type FetchFn = (
  url: string,
  init: { cache: "no-store"; credentials: "omit"; redirect: "error" },
) => Promise<{
  ok: boolean;
  status: number;
  headers: { get(name: string): string | null };
  text(): Promise<string>;
}>;

/** Fetches and checks `/config.json`; every failure is an Error naming it. */
export async function loadRuntimeRelay(fetchFn: FetchFn, pageProtocol: string): Promise<string> {
  let body: string;
  try {
    const res = await fetchFn("/config.json", { cache: "no-store", credentials: "omit", redirect: "error" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const type = res.headers.get("content-type") ?? "";
    if (type.split(";")[0]?.trim().toLowerCase() !== "application/json") throw new Error(`content type ${type}`);
    body = await res.text();
  } catch (e) {
    throw new Error(`cannot load /config.json: ${e instanceof Error ? e.message : String(e)}`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    throw new Error("bad /config.json: not JSON");
  }
  return relayFromConfig(parsed, pageProtocol);
}
