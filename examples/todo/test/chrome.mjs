// SPDX-License-Identifier: MIT
// Shared by the headless-Chrome tests (browser.test.mjs, image.test.mjs): finding
// Chrome, free ports, kept-output child processes, and a minimal DevTools protocol
// client over Node 22's global WebSocket (no npm dependency).

import { spawn, spawnSync } from "node:child_process";
import { createServer as createNetServer } from "node:net";

import { withTimeout } from "../../../vault-web/js/test/helpers.mjs";

export function findChrome() {
  if (process.env.CHROME_BIN) return process.env.CHROME_BIN;
  for (const name of ["google-chrome", "google-chrome-stable", "chrome", "chromium", "chromium-browser"]) {
    const r = spawnSync("sh", ["-c", `command -v ${name}`], { encoding: "utf8" });
    if (r.status === 0 && r.stdout.trim()) return r.stdout.trim();
  }
  throw new Error("Chrome not found: set CHROME_BIN or put google-chrome/chrome/chromium on PATH");
}

export function freePort() {
  return new Promise((resolve, reject) => {
    const s = createNetServer();
    s.on("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

/** A process whose output is kept; `waitFor(re)` resolves with the first match. */
export function startProc(procs, name, cmd, args, env = {}) {
  const child = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, ...env } });
  const p = { name, child, out: "", listeners: new Set() };
  const onData = (d) => {
    p.out += d;
    for (const l of p.listeners) l();
  };
  child.stdout.setEncoding("utf8").on("data", onData);
  child.stderr.setEncoding("utf8").on("data", onData);
  child.on("exit", (code, sig) => onData(`\n[${name} exited: ${code} ${sig}]\n`));
  p.waitFor = (re, ms, what) =>
    withTimeout(
      new Promise((resolve) => {
        const check = () => {
          const m = p.out.match(re);
          if (m) {
            p.listeners.delete(check);
            resolve(m);
          }
        };
        p.listeners.add(check);
        check();
      }),
      ms,
      `${what} (from ${name}); output so far:\n${p.out.slice(-3000)}`,
    );
  procs.push(p);
  return p;
}

/** A minimal CDP client over the browser WebSocket (flattened target sessions). */
export async function cdpConnect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve, { once: true });
    ws.addEventListener("error", () => reject(new Error(`cannot connect to ${wsUrl}`)), { once: true });
  });
  let next = 1;
  const pending = new Map();
  const handlers = new Set();
  ws.addEventListener("message", (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) reject(new Error(`${msg.error.message} (${msg.error.code})`));
      else resolve(msg.result);
    } else if (msg.method) {
      for (const h of handlers) h(msg);
    }
  });
  return {
    send(method, params = {}, sessionId) {
      const id = next++;
      ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
      return withTimeout(
        new Promise((resolve, reject) => pending.set(id, { resolve, reject })),
        15_000,
        `CDP ${method}`,
      );
    },
    on(h) {
      handlers.add(h);
    },
    close() {
      ws.close();
    },
  };
}

// Injected before any page script: record CSP violations and report them as errors.
export const VIOLATION_LISTENER = `
  window.__cspViolations = [];
  document.addEventListener("securitypolicyviolation", (e) => {
    const v = { directive: e.violatedDirective, blocked: e.blockedURI, policy: e.originalPolicy };
    window.__cspViolations.push(v);
    console.error("CSP violation: " + JSON.stringify(v));
  });
`;

/** Resolves once `p` exited; SIGKILL after `graceMs` if it has not by then. */
export function exited(p, graceMs) {
  return new Promise((resolve) => {
    if (p.child.exitCode !== null || p.child.signalCode !== null) return resolve();
    const timer = setTimeout(() => p.child.kill("SIGKILL"), graceMs);
    p.child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}


/** Chrome, headless, on a throwaway profile; resolves with the process and a CDP client. */
export async function startChrome(procs, chromeBin, profileDir) {
  const chrome = startProc(procs, chromeBin, chromeBin, [
    "--headless=new",
    "--no-sandbox",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--remote-debugging-port=0",
    `--user-data-dir=${profileDir}`,
    "about:blank",
  ]);
  const dt = await chrome.waitFor(/DevTools listening on (ws:\/\/\S+)/, 30_000, "Chrome's DevTools endpoint");
  return { proc: chrome, cdp: await cdpConnect(dt[1]) };
}

/**
 * A new tab with console errors, exceptions and CSP violations collected into
 * `errors`; `evaluate` runs an expression in it, `waitUntil` polls one.
 */
export async function openTab(cdp) {
  const errors = [];
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  const responses = [];
  cdp.on((msg) => {
    if (msg.sessionId !== sessionId) return;
    if (msg.method === "Runtime.consoleAPICalled" && (msg.params.type === "error" || msg.params.type === "assert")) {
      errors.push(`console.${msg.params.type}: ${msg.params.args.map((a) => a.value ?? a.description).join(" ")}`);
    } else if (msg.method === "Runtime.exceptionThrown") {
      errors.push(`exception: ${msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text}`);
    } else if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") {
      errors.push(`log (${msg.params.entry.source}): ${msg.params.entry.text}`);
    } else if (msg.method === "Network.responseReceived") {
      const r = msg.params.response;
      responses.push({ url: r.url, status: r.status, type: r.mimeType });
    }
  });
  const send = (m, p) => cdp.send(m, p, sessionId);
  await send("Runtime.enable");
  await send("Log.enable");
  await send("Network.enable");
  await send("Page.enable");
  await send("Page.addScriptToEvaluateOnNewDocument", { source: VIOLATION_LISTENER });
  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(`evaluate failed: ${r.exceptionDetails.text}`);
    return r.result.value;
  };
  const waitUntil = async (expression, what, ms = 20_000) => {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      if (await evaluate(expression)) return;
      await new Promise((r) => setTimeout(r, 100));
    }
    const body = await evaluate("document.body?.innerText ?? ''").catch(() => "?");
    throw new Error(`timed out waiting for ${what}; errors: ${JSON.stringify(errors)}; page: ${body}`);
  };
  return {
    errors,
    responses,
    send,
    evaluate,
    waitUntil,
    navigate: (url) => send("Page.navigate", { url }),
    close: () => cdp.send("Target.closeTarget", { targetId }),
  };
}
