// SPDX-License-Identifier: MIT
// End-to-end in LAN mode (scripts/showcase.sh --lan): the real vault-relay
// terminating TLS (wss), the page built for that relay and served over https by
// serve.mjs, and vault-dev trusting the local CA with --ca-file, all with a CA
// and certificates made by scripts/lan-cert.sh. "127.0.0.1" stands in for the
// LAN IP. Node trusts the CA through NODE_EXTRA_CA_CERTS, which it reads only at
// start: run this file through test/lan-run.mjs (npm run test:lan), which makes
// the CA and sets TODO_LAN_CA_DIR.
//
// Needs vault-web built (in vault-web/js: npm ci, npm run build, npm run build:wasm)
// and the binaries (built with cargo, or VAULT_BIN_DIR).

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { get as httpsGet } from "node:https";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { after, before, describe, test } from "node:test";

import { APP_ID, Proc, binaries, withHarness, withTimeout } from "../../../vault-web/js/test/helpers.mjs";
import { TodoApp } from "../.test-build/app.js";

const here = dirname(fileURLToPath(import.meta.url));
const caDir = process.env.TODO_LAN_CA_DIR;
if (!caDir || !process.env.NODE_EXTRA_CA_CERTS) {
  throw new Error("run this through test/lan-run.mjs (npm run test:lan): it makes the CA and trusts it");
}
const CA = join(caDir, "ca.pem");
const IP = "127.0.0.1";
const WRONG_IP = "127.0.0.2";
const leaf = (ip) => ({ cert: join(caDir, `leaf-${ip}.pem`), key: join(caDir, `leaf-${ip}.key`) });

// Fails with a build hint if the wasm package is missing.
const { pair } = await import("vault-web/node");

const T = { timeout: 180_000 };
const ctx = {};

/** A free loopback port (for the page origin, which the relay must know first). */
function freePort() {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.on("error", reject);
    s.listen(0, IP, () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

/** vault-relay on 127.0.0.1 with TLS from the certificate issued for `certIp`. */
async function startTlsRelay(certIp, allowOrigin) {
  const { cert, key } = leaf(certIp);
  const p = new Proc(
    `vault-relay (TLS, certificate for ${certIp})`,
    ctx.bins.relay,
    ["--listen", `${IP}:0`, "--allow-origin", allowOrigin, "--tls-cert", cert, "--tls-key", key],
    { env: { RUST_LOG: "info" } },
  );
  const m = await p.waitFor(
    /vault-relay listening[^\n]*?addr=(127\.0\.0\.1:\d+)[^\n]*tls=true/,
    20_000,
    "the relay to listen",
  );
  return { proc: p, addr: m[1], stop: () => p.kill(), text: () => p.text() };
}

/** GET over https with Node's default trust (system CAs plus NODE_EXTRA_CA_CERTS). */
function get(url) {
  return withTimeout(
    new Promise((resolve, reject) => {
      httpsGet(url, (res) => {
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (d) => (body += d));
        res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, body }));
      }).on("error", reject);
    }),
    15_000,
    `GET ${url}`,
  );
}

/** Records the screens; `waitFor` resolves with the first matching one from `from` on. */
class RecordingUi {
  constructor() {
    this.screens = [];
    this.listeners = new Set();
  }
  #push(s) {
    this.screens.push(s);
    for (const l of this.listeners) l();
  }
  showStart(h) {
    this.#push({ name: "start", h });
  }
  showOffer(offer, h) {
    this.#push({ name: "offer", offer, h });
  }
  showSas(sas, h) {
    this.#push({ name: "sas", sas, h });
  }
  showList(model, h) {
    this.#push({ name: "list", model: structuredClone(model), h });
  }
  showClosed(message, h) {
    this.#push({ name: "closed", message, h });
  }
  get last() {
    return this.screens.at(-1);
  }
  mark() {
    return this.screens.length;
  }
  waitFor(pred, what, from = 0, ms = 30_000) {
    return withTimeout(
      new Promise((resolve) => {
        const check = () => {
          const s = this.screens.slice(from).find(pred);
          if (s) {
            this.listeners.delete(check);
            resolve(s);
          }
        };
        this.listeners.add(check);
        check();
      }),
      ms,
      `${what}; screens so far: ${JSON.stringify(this.screens.map((s) => s.name))}`,
    );
  }
}

before(() => {
  ctx.bins = binaries();
  ctx.tmp = mkdtempSync(join(tmpdir(), "todo-lan-e2e-"));
});

after(() => {
  ctx.relay?.stop();
  ctx.web?.kill();
  if (ctx.tmp) rmSync(ctx.tmp, { recursive: true, force: true });
});

describe("todo e2e, LAN mode", () => {
  test("lan_mode_e2e", T, async () => {
    const webPort = await freePort();
    const pageOrigin = `https://${IP}:${webPort}`;
    ctx.relay = await startTlsRelay(IP, pageOrigin);
    const relayOrigin = `https://${ctx.relay.addr}`;

    // The page: release build for the wss relay, served over https.
    const out = join(ctx.tmp, "web");
    const b = spawnSync(
      process.execPath,
      [join(here, "..", "build.mjs"), "--relay", `wss://${ctx.relay.addr}`, "--page-origin", pageOrigin, "--out", out],
      { encoding: "utf8" },
    );
    assert.equal(b.status, 0, `build.mjs failed:\n${b.stderr}`);
    const { cert, key } = leaf(IP);
    ctx.web = new Proc("serve.mjs (https)", process.execPath, [
      join(here, "..", "serve.mjs"),
      "--port",
      String(webPort),
      "--dir",
      out,
      "--tls-cert",
      cert,
      "--tls-key",
      key,
    ]);
    await ctx.web.waitFor(/serving .* at https:\/\/localhost:\d+\//, 20_000, "the page server");
    const page = await get(`${pageOrigin}/`);
    assert.equal(page.status, 200);
    assert.ok(
      page.headers["content-security-policy"].includes(`connect-src wss://${ctx.relay.addr};`),
      page.headers["content-security-policy"],
    );
    assert.match(page.body, /<script[^>]*src="main\.js"/);
    const health = await get(`${relayOrigin}/healthz`);
    assert.equal(health.status, 200, "the relay answers over https with the local CA");

    await withHarness(ctx, async (h) => {
      const vault = h.initVault({ origin: pageOrigin });
      const ui = new RecordingUi();
      const app = new TodoApp({
        pair: (opts) => pair({ ...opts, testOrigin: pageOrigin }),
        ui,
        relayOrigin,
        deviceName: "todo lan e2e",
        dev: false,
        warn: () => {},
      });

      const pairApp = async () => {
        const from = ui.mark();
        await app.connect();
        const offer = await ui.waitFor((s) => s.name === "offer", "the QR screen", from);
        // vault-dev is the phone: it dials wss:// and trusts the local CA only via --ca-file.
        const vd = h.serveVault(vault, offer.offer.text, ["--ca-file", CA]);
        const sas = await ui.waitFor((s) => s.name === "sas", "the code screen", from);
        const prompt = await vd.prompt();
        assert.equal(prompt.code, sas.sas, "the vault and the page show the same code");
        vd.answer(`y ${prompt.n}`);
        sas.h.onMatch();
        await ui.waitFor((s) => s.name === "list" && s.model.ready, "the loaded list", from);
        return vd;
      };
      const lock = async (vd) => {
        const from = ui.mark();
        ui.last.h.onLock();
        await ui.waitFor((s) => s.name === "closed", "the closed screen", from);
        assert.equal((await vd.exit()).code, 0, "vault-dev exit status");
      };

      const a = await pairApp();
      assert.deepEqual(ui.last.model.todos, [], "a new vault is empty");
      ui.last.h.onAdd("buy milk on the LAN");
      await app.list.idle();
      assert.equal(ui.last.model.status, "saved");
      await lock(a);

      const c = await pairApp();
      assert.deepEqual(
        ui.last.model.todos.map((t) => [t.text, t.done]),
        [["buy milk on the LAN", false]],
        "persisted across sessions",
      );
      await lock(c);
    });
  });

  test("lan_wrong_ip_cert_refused", T, async () => {
    // The same CA, but the relay presents the certificate for another IP.
    const pageOrigin = `https://${IP}:${await freePort()}`;
    const wrong = await startTlsRelay(WRONG_IP, pageOrigin);
    try {
      await assert.rejects(get(`https://${wrong.addr}/healthz`), (e) => {
        assert.equal(e.code, "ERR_TLS_CERT_ALTNAME_INVALID", `${e.code}: ${e.message}`);
        return true;
      });

      await withHarness({ ...ctx, relay: wrong }, async (h) => {
        const vault = h.initVault({ origin: pageOrigin });
        const probe = h.probe();
        const session = await pair({
          relayOrigin: `https://${wrong.addr}`,
          appId: APP_ID,
          deviceName: "todo lan wrong ip",
          testOrigin: pageOrigin,
          ...probe.callbacks(),
        });
        const offer = await probe.offer();
        // Node's WebSocket refuses the relay's certificate: never paired.
        await probe.state("closed");
        assert.ok(!probe.events.some((e) => e.kind === "sas"), "no handshake");
        // vault-dev trusts the CA, yet refuses a certificate for another IP.
        const vd = h.serveVault(vault, offer.text, ["--ca-file", CA]);
        const st = await vd.exit();
        assert.notEqual(st.code, 0, vd.text());
        assert.match(vd.text(), /cannot reach the relay/);
        assert.doesNotMatch(vd.text(), /Connected\./);
        assert.ok(!vd.hasPrompted(), "the vault never asks the user");
        await session.lock();
      });
    } finally {
      wrong.stop();
    }
  });
});
