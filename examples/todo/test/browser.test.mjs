// SPDX-License-Identifier: MIT
// Real-browser smoke test: headless Chrome loads the built page from serve.mjs
// (with its CSP header and meta), clicks "Connect to vault", and the page's
// WebSocket must reach the real vault-relay (an upgrade on /mailbox) with no CSP
// violation and no console error. Chrome is driven over the DevTools protocol
// with Node 22's global WebSocket; no npm dependency.
//
// Needs vault-web built (pkg/web), the vault-relay binary (VAULT_BIN_DIR or
// cargo), and Chrome: CHROME_BIN, or google-chrome / chrome / chromium on PATH.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { rm } from "node:fs/promises";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { binaries, withTimeout } from "../../../vault-web/js/test/helpers.mjs";
import { build, parseArgs } from "../build.mjs";
import { createStaticServer } from "../serve.mjs";

const T = { timeout: 120_000 };
const ctx = { procs: [], dirs: [] };

function findChrome() {
  if (process.env.CHROME_BIN) return process.env.CHROME_BIN;
  for (const name of ["google-chrome", "google-chrome-stable", "chrome", "chromium", "chromium-browser"]) {
    const r = spawnSync("sh", ["-c", `command -v ${name}`], { encoding: "utf8" });
    if (r.status === 0 && r.stdout.trim()) return r.stdout.trim();
  }
  throw new Error("Chrome not found: set CHROME_BIN or put google-chrome/chrome/chromium on PATH");
}

function freePort() {
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
function startProc(name, cmd, args, env = {}) {
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
  ctx.procs.push(p);
  return p;
}

/** A minimal CDP client over the browser WebSocket (flattened target sessions). */
async function cdpConnect(wsUrl) {
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
const VIOLATION_LISTENER = `
  window.__cspViolations = [];
  document.addEventListener("securitypolicyviolation", (e) => {
    const v = { directive: e.violatedDirective, blocked: e.blockedURI, policy: e.originalPolicy };
    window.__cspViolations.push(v);
    console.error("CSP violation: " + JSON.stringify(v));
  });
`;

before(async () => {
  ctx.bins = binaries();
  ctx.chrome = findChrome();
  ctx.pagePort = await freePort();
  ctx.pageOrigin = `http://localhost:${ctx.pagePort}`;
  const relay = startProc(
    "vault-relay",
    ctx.bins.relay,
    ["--listen", "127.0.0.1:0", "--allow-origin", ctx.pageOrigin],
    { RUST_LOG: "info", NO_COLOR: "1" },
  );
  const m = await relay.waitFor(/vault-relay listening[^\n]*?addr=(127\.0\.0\.1:\d+)/, 20_000, "the relay to listen");
  ctx.relay = relay;
  ctx.relayOrigin = `http://${m[1]}`;

  const base = mkdtempSync(join(tmpdir(), "todo-browser-"));
  ctx.dirs.push(base);
  const dist = join(base, "dist");
  await build(parseArgs(["--relay", ctx.relayOrigin, "--out", dist]));
  ctx.server = createStaticServer({ root: dist, relayOrigin: ctx.relayOrigin });
  await new Promise((r) => ctx.server.listen(ctx.pagePort, "127.0.0.1", r));

  const chrome = startProc(ctx.chrome, ctx.chrome, [
    "--headless=new",
    "--no-sandbox",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--remote-debugging-port=0",
    `--user-data-dir=${join(base, "profile")}`,
    "about:blank",
  ]);
  const dt = await chrome.waitFor(/DevTools listening on (ws:\/\/\S+)/, 30_000, "Chrome's DevTools endpoint");
  ctx.chromeProc = chrome;
  ctx.cdp = await cdpConnect(dt[1]);
});

/** Resolves once `p` exited; SIGKILL after `graceMs` if it has not by then. */
function exited(p, graceMs) {
  return new Promise((resolve) => {
    if (p.child.exitCode !== null || p.child.signalCode !== null) return resolve();
    const timer = setTimeout(() => p.child.kill("SIGKILL"), graceMs);
    p.child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

after(async () => {
  // Chrome first: ask it to quit over CDP and wait for the process, so it is no
  // longer writing its profile when the temp dir is removed.
  if (ctx.cdp) {
    try {
      await ctx.cdp.send("Browser.close");
    } catch (e) {
      // The connection may drop before Chrome answers; the exit wait decides.
      console.warn(`browser.test: Browser.close: ${e.message}`);
    }
    ctx.cdp.close();
  }
  if (ctx.chromeProc) await exited(ctx.chromeProc, 5_000);
  ctx.server?.close();
  for (const p of ctx.procs) if (p !== ctx.chromeProc) p.child.kill("SIGKILL");
  await Promise.all(ctx.procs.map((p) => exited(p, 5_000)));
  for (const d of ctx.dirs) {
    try {
      await rm(d, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch (e) {
      // A leftover temp dir must not turn a passing run red (or hide a failure),
      // but say so; anything else is a real error.
      if (["ENOTEMPTY", "EBUSY", "EPERM"].includes(e.code)) {
        console.warn(`browser.test: could not remove ${d}: ${e.code}`);
      } else {
        throw e;
      }
    }
  }
});

test("browser_smoke_csp_and_upgrade", T, async () => {
  const { cdp } = ctx;
  const errors = [];
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  cdp.on((msg) => {
    if (msg.sessionId !== sessionId) return;
    if (msg.method === "Runtime.consoleAPICalled" && (msg.params.type === "error" || msg.params.type === "assert")) {
      errors.push(`console.${msg.params.type}: ${msg.params.args.map((a) => a.value ?? a.description).join(" ")}`);
    } else if (msg.method === "Runtime.exceptionThrown") {
      errors.push(`exception: ${msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text}`);
    } else if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") {
      errors.push(`log (${msg.params.entry.source}): ${msg.params.entry.text}`);
    }
  });
  const send = (m, p) => cdp.send(m, p, sessionId);
  await send("Runtime.enable");
  await send("Log.enable");
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
    throw new Error(`timed out waiting for ${what}; errors: ${JSON.stringify(errors)}`);
  };

  const mailboxesBefore = (ctx.relay.out.match(/mailbox open/g) ?? []).length;
  await send("Page.navigate", { url: `${ctx.pageOrigin}/` });
  await waitUntil(
    `[...document.querySelectorAll("button")].some((b) => b.textContent === "Connect to vault")`,
    "the start screen",
  );
  assert.equal(await evaluate("location.origin"), ctx.pageOrigin);
  await evaluate(
    `[...document.querySelectorAll("button")].find((b) => b.textContent === "Connect to vault").click()`,
  );
  // The QR screen: the page paired far enough to have an offer.
  await waitUntil(`!!document.querySelector("svg.qr") || window.__cspViolations.length > 0`, "the QR screen");
  // And the relay saw the page's WebSocket upgrade on /mailbox (or a violation stopped it).
  const deadline = Date.now() + 15_000;
  while (
    (ctx.relay.out.match(/mailbox open/g) ?? []).length === mailboxesBefore &&
    (await evaluate("window.__cspViolations.length")) === 0 &&
    Date.now() < deadline
  ) {
    await new Promise((r) => setTimeout(r, 100));
  }
  const violations = await evaluate("window.__cspViolations");
  assert.deepEqual(violations, [], "no CSP violation");
  assert.deepEqual(errors, [], "no console error");
  assert.ok(
    (ctx.relay.out.match(/mailbox open/g) ?? []).length > mailboxesBefore,
    `the relay saw a WebSocket upgrade on /mailbox; relay output:\n${ctx.relay.out.slice(-2000)}`,
  );
  assert.equal(await evaluate(`!!document.querySelector("svg.qr")`), true, "the QR is shown");
  await cdp.send("Target.closeTarget", { targetId });
});
