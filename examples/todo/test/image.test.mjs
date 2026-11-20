// SPDX-License-Identifier: MIT
// Image mode end to end (what the vault-todo-demo image runs): the
// `build.mjs --runtime-config` page served by `vault-relay --static-dir` on the
// relay's own port, in headless Chrome, with only the relay's CSP header (no
// <meta>). The page must fetch and accept /config.json, show no CSP violation or
// console error, and open a mailbox on the same relay. Then vault-dev, the phone
// stand-in, pairs with the text decoded from the QR the page drew (with rqrr, an
// independent decoder, as a camera would), an item is added, the page locks, and
// a second pairing shows the item again: it lives in the vault.
//
// Needs vault-web built (pkg/web), the binaries (VAULT_BIN_DIR or cargo) plus the
// qr_decode example (cargo build -p vault-web --example qr_decode; looked up in
// VAULT_BIN_DIR/examples), and Chrome (CHROME_BIN or on PATH).

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { Harness, binaries, repoRoot } from "../../../vault-web/js/test/helpers.mjs";
import { build, parseArgs } from "../build.mjs";
import { exited, findChrome, freePort, openTab, startChrome, startProc } from "./chrome.mjs";
import { qrRowsFromSvg } from "./qr-svg.mjs";

const T = { timeout: 180_000 };
const ctx = { procs: [], dirs: [] };
const EXE = process.platform === "win32" ? ".exe" : "";

/** The qr_decode example binary (next to the binaries, or built with cargo). */
function qrDecoder() {
  if (process.env.VAULT_BIN_DIR) {
    const p = join(process.env.VAULT_BIN_DIR, "examples", `qr_decode${EXE}`);
    if (!existsSync(p)) throw new Error(`${p} missing: cargo build --locked -p vault-web --example qr_decode`);
    return p;
  }
  const r = spawnSync("cargo", ["build", "--locked", "-p", "vault-web", "--example", "qr_decode", "--message-format=json"], {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (r.status !== 0) throw new Error(`cargo build of qr_decode failed:\n${r.stderr}`);
  const exe = r.stdout
    .split("\n")
    .filter((l) => l.startsWith("{"))
    .map((l) => JSON.parse(l))
    .find((m) => m.reason === "compiler-artifact" && m.target?.name === "qr_decode" && m.executable)?.executable;
  if (!exe) throw new Error("qr_decode executable not found in cargo's output");
  return exe;
}

before(async () => {
  ctx.bins = binaries();
  ctx.decoder = qrDecoder();
  ctx.chrome = findChrome();
  const port = await freePort();
  ctx.origin = `http://localhost:${port}`;
  const base = mkdtempSync(join(tmpdir(), "todo-image-"));
  ctx.dirs.push(base);
  ctx.dist = join(base, "dist");
  await build(parseArgs(["--runtime-config", "--out", ctx.dist]));
  // As the image's entrypoint does, with the loopback http origin a local run may use.
  ctx.relay = startProc(
    ctx.procs,
    "vault-relay",
    ctx.bins.relay,
    ["--listen", `127.0.0.1:${port}`, "--static-dir", ctx.dist, "--public-origin", ctx.origin],
    { RUST_LOG: "info", NO_COLOR: "1" },
  );
  await ctx.relay.waitFor(/vault-relay listening/, 20_000, "the relay to listen");
  const chrome = await startChrome(ctx.procs, ctx.chrome, join(base, "profile"));
  ctx.chromeProc = chrome.proc;
  ctx.cdp = chrome.cdp;
});

after(async () => {
  if (ctx.cdp) {
    try {
      await ctx.cdp.send("Browser.close");
    } catch (e) {
      console.warn(`image.test: Browser.close: ${e.message}`);
    }
    ctx.cdp.close();
  }
  if (ctx.chromeProc) await exited(ctx.chromeProc, 5_000);
  for (const p of ctx.procs) if (p !== ctx.chromeProc) p.child.kill("SIGKILL");
  await Promise.all(ctx.procs.map((p) => exited(p, 5_000)));
  for (const d of ctx.dirs) {
    try {
      await rm(d, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch (e) {
      if (["ENOTEMPTY", "EBUSY", "EPERM"].includes(e.code)) console.warn(`image.test: could not remove ${d}: ${e.code}`);
      else throw e;
    }
  }
});

const BUTTON = (text) => `[...document.querySelectorAll("button")].find((b) => b.textContent === ${JSON.stringify(text)})`;
const mailboxes = () => (ctx.relay.out.match(/mailbox open/g) ?? []).length;

/** Clicks `button`, reads the QR the page shows, and returns its decoded text. */
async function offerText(tab, button) {
  const before = mailboxes();
  await tab.waitUntil(`!!${BUTTON(button)}`, `the "${button}" button`);
  await tab.evaluate(`${BUTTON(button)}.click()`);
  await tab.waitUntil(`!!document.querySelector("svg.qr") || window.__cspViolations.length > 0`, "the QR screen");
  const deadline = Date.now() + 15_000;
  while (mailboxes() === before && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
  assert.deepEqual(await tab.evaluate("window.__cspViolations"), [], "no CSP violation");
  assert.ok(mailboxes() > before, `the relay saw the page's /mailbox upgrade:\n${ctx.relay.out.slice(-2000)}`);
  const { viewBox, d } = await tab.evaluate(
    `(() => { const s = document.querySelector("svg.qr"); return { viewBox: s.getAttribute("viewBox"), d: s.querySelector("path").getAttribute("d") }; })()`,
  );
  const rows = qrRowsFromSvg(viewBox, d);
  const r = spawnSync(ctx.decoder, [], { input: `${rows.join("\n")}\n`, encoding: "utf8" });
  assert.equal(r.status, 0, `qr_decode: ${r.stderr}`);
  const text = r.stdout.trim();
  assert.match(text, /^vault-pair:v1:/, "the QR holds the pairing text");
  return text;
}

/** vault-dev for `text`; the codes are compared, approved, and the list loads. */
async function pairVault(h, tab, vault, text) {
  const vd = h.serveVault(vault, text);
  await tab.waitUntil(`!!document.querySelector("p.sas")`, "the code screen");
  const prompt = await vd.prompt();
  const shown = await tab.evaluate(`document.querySelector("p.sas").textContent`);
  assert.equal(shown.replace(/\D/g, ""), prompt.code.replace(/\D/g, ""), "the page and the vault show the same code");
  vd.answer(`y ${prompt.n}`);
  await tab.evaluate(`${BUTTON("Codes match")}.click()`);
  await tab.waitUntil(
    `(() => { const i = document.querySelector("form.add input"); return !!i && !i.disabled; })()`,
    "the loaded list",
    30_000,
  );
  return vd;
}

const items = (tab) => tab.evaluate(`[...document.querySelectorAll("ul.todos li .text")].map((s) => s.textContent)`);

test("image_mode_browser_e2e", T, async () => {
  const h = new Harness(ctx.bins, null);
  const tab = await openTab(ctx.cdp);
  try {
    const vault = h.initVault({ origin: ctx.origin });
    await tab.navigate(`${ctx.origin}/`);
    await tab.waitUntil(`!!${BUTTON("Connect to vault")} || !!document.querySelector("p.notice")`, "the start screen");
    assert.equal(
      await tab.evaluate(`document.querySelector("p.notice")?.textContent ?? ""`),
      "",
      "the page accepted /config.json",
    );
    // The page got its relay from /config.json, under the header CSP only.
    const cfg = tab.responses.filter((r) => r.url === `${ctx.origin}/config.json`);
    assert.deepEqual(
      cfg.map((r) => [r.status, r.type]),
      [[200, "application/json"]],
      "one /config.json fetch",
    );
    assert.equal(await tab.evaluate(`document.querySelectorAll('meta[http-equiv="Content-Security-Policy"]').length`), 0);

    // Pair, add an item, lock.
    const vd1 = await pairVault(h, tab, vault, await offerText(tab, "Connect to vault"));
    const item = `image mode ${Date.now()}`;
    await tab.evaluate(
      `(() => { const i = document.querySelector("form.add input"); i.value = ${JSON.stringify(item)}; document.querySelector("form.add").requestSubmit(); })()`,
    );
    // The edit shows "saving…" at once; "saved" with the item means it was committed.
    await tab.waitUntil(
      `document.querySelector("p.status")?.textContent === "saved" && [...document.querySelectorAll("ul.todos li .text")].some((s) => s.textContent === ${JSON.stringify(item)})`,
      "the commit",
    );
    assert.deepEqual(await items(tab), [item]);
    await tab.evaluate(`${BUTTON("Lock")}.click()`);
    await tab.waitUntil(`document.querySelector("p.notice")?.textContent === "Vault locked."`, "the locked notice");
    assert.equal((await vd1.exit()).code, 0, "vault-dev exit status");

    // Pair again: the item comes back from the vault.
    const vd2 = await pairVault(h, tab, vault, await offerText(tab, "Pair again"));
    await tab.waitUntil(`document.querySelectorAll("ul.todos li").length > 0`, "the stored item");
    assert.deepEqual(await items(tab), [item], "the item persisted in the vault");
    await tab.evaluate(`${BUTTON("Lock")}.click()`);
    await tab.waitUntil(`document.querySelector("p.notice")?.textContent === "Vault locked."`, "the second lock");
    assert.equal((await vd2.exit()).code, 0);

    assert.deepEqual(await tab.evaluate("window.__cspViolations"), [], "no CSP violation");
    assert.deepEqual(tab.errors, [], "no console error");
  } catch (e) {
    const report = `\n===== process output =====\n${h.dump()}\n----- vault-relay -----\n${ctx.relay.out.slice(-3000)}`;
    if (e instanceof Error) e.message += report;
    throw e;
  } finally {
    await tab.close().catch(() => {});
    h.cleanup();
  }
});
