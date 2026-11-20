// SPDX-License-Identifier: MIT
// End-to-end helpers: build or locate the real binaries (vault-relay, vault-dev,
// vaultctl), create temporary vaults, run the relay and vault-dev, and record what
// the client reports. Every helper fails with a message that says what it waited
// for; `Harness.dump()` collects the processes' output for the failure report.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
export const repoRoot = resolve(here, "../../..");
export const APP_ID = "vault-todo";
/** The page origin the vaults are bound to; passed to `pair` as `testOrigin`. */
export const TEST_ORIGIN = "http://localhost:8080";
const PASSPHRASE = "e2e test passphrase";
const EXE = process.platform === "win32" ? ".exe" : "";

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** `promise`, or a rejection naming `what` after `ms`. */
export function withTimeout(promise, ms, what) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(
      () => reject(new Error(`timed out after ${ms} ms waiting for ${what}`)),
      ms,
    );
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/**
 * Paths of the three binaries. `VAULT_BIN_DIR` names a directory that has them;
 * otherwise they are built with `cargo build --locked` (a no-op when CI built them
 * in an earlier step).
 */
export function binaries() {
  let dir = process.env.VAULT_BIN_DIR;
  if (!dir) {
    const build = spawnSync(
      "cargo",
      ["build", "--locked", "--bin", "vault-relay", "--bin", "vault-dev", "--bin", "vaultctl"],
      { cwd: repoRoot, stdio: ["ignore", "inherit", "inherit"] },
    );
    if (build.error || build.status !== 0) {
      throw new Error(
        `cargo build of the binaries failed (status ${build.status}): ${build.error ?? "see the output above"}`,
      );
    }
    const meta = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
      cwd: repoRoot,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
    });
    if (meta.status !== 0) throw new Error(`cargo metadata failed: ${meta.stderr}`);
    dir = join(JSON.parse(meta.stdout).target_directory, "debug");
  }
  const bins = {
    relay: join(dir, `vault-relay${EXE}`),
    dev: join(dir, `vault-dev${EXE}`),
    ctl: join(dir, `vaultctl${EXE}`),
  };
  for (const [name, path] of Object.entries(bins)) {
    if (!existsSync(path)) throw new Error(`binary ${name} not found at ${path}`);
  }
  return bins;
}

/** A child process whose stdout and stderr are kept for waiting and for the report. */
export class Proc {
  constructor(name, cmd, args, opts = {}) {
    this.name = name;
    this.out = "";
    this.listeners = new Set();
    this.child = spawn(cmd, args, {
      stdio: ["pipe", "pipe", "pipe"],
      ...opts,
      env: { ...process.env, NO_COLOR: "1", RUST_BACKTRACE: "1", ...(opts.env ?? {}) },
    });
    this.exitInfo = undefined;
    this.exited = new Promise((resolveExit) => {
      this.child.on("exit", (code, signal) => {
        this.exitInfo = { code, signal };
        this.append(`\n[${this.name} exited: code ${code}, signal ${signal}]\n`);
        resolveExit(this.exitInfo);
      });
      this.child.on("error", (e) => {
        this.exitInfo = { code: null, signal: null, error: e };
        this.append(`\n[${this.name} failed to start: ${e}]\n`);
        resolveExit(this.exitInfo);
      });
    });
    this.child.stdout.setEncoding("utf8").on("data", (d) => this.append(d));
    this.child.stderr.setEncoding("utf8").on("data", (d) => this.append(d));
    // Writes to a vault-dev that already exited must not crash the test process.
    this.child.stdin.on("error", () => {});
  }

  append(text) {
    this.out += text;
    for (const l of this.listeners) l();
  }

  /** The output so far, without terminal escape sequences. */
  text() {
    // eslint-disable-next-line no-control-regex
    return this.out.replace(/\x1b\[[0-9;]*m/g, "");
  }

  /** Resolves with the first match of `re` in the output (also already printed). */
  waitFor(re, ms, what) {
    return withTimeout(
      new Promise((resolveMatch, reject) => {
        const check = () => {
          const m = this.text().match(re);
          if (m) {
            this.listeners.delete(check);
            resolveMatch(m);
          } else if (this.exitInfo) {
            this.listeners.delete(check);
            reject(new Error(`${this.name} exited (${JSON.stringify(this.exitInfo)}) before ${what}`));
          }
        };
        this.listeners.add(check);
        check();
        this.exited.then(check);
      }),
      ms,
      `${what} (from ${this.name})`,
    );
  }

  write(line) {
    this.child.stdin.write(`${line}\n`);
  }

  /** Resolves with `{ code, signal }` once the process exited. */
  exit(ms) {
    return withTimeout(this.exited, ms, `${this.name} to exit`);
  }

  kill() {
    if (!this.exitInfo) {
      this.child.stdin.end();
      this.child.kill("SIGKILL");
    }
  }
}

/** Per-test state: the processes and temp dirs to clean up and to report on failure. */
export class Harness {
  constructor(bins, relay) {
    this.bins = bins;
    this.relay = relay;
    this.procs = [];
    this.dirs = [];
    this.probes = [];
  }

  /** A new client probe, included in the failure report. */
  probe() {
    const p = new Probe();
    this.probes.push(p);
    return p;
  }

  /** Every process's output (the relay's last 150 lines), for a failure report. */
  dump() {
    const parts = this.probes.map((p, i) => `----- client session #${i + 1} events -----\n${p.describe()}`);
    parts.push(...this.procs.map((p) => `----- ${p.name} -----\n${p.text()}`));
    if (this.relay) {
      const lines = this.relay.text().split("\n");
      parts.push(`----- vault-relay (last 150 lines) -----\n${lines.slice(-150).join("\n")}`);
    }
    return parts.join("\n");
  }

  cleanup() {
    for (const p of this.procs) p.kill();
    for (const d of this.dirs) rmSync(d, { recursive: true, force: true });
  }

  /**
   * A new container bound to `app` at `origin` (`vaultctl init`), with its
   * passphrase in a file for vault-dev's `--passphrase-file`.
   */
  initVault({ app = APP_ID, origin = TEST_ORIGIN } = {}) {
    const base = mkdtempSync(join(tmpdir(), "vault-web-e2e-"));
    this.dirs.push(base);
    const dir = join(base, "vault");
    const passFile = join(base, "passphrase");
    writeFileSync(passFile, `${PASSPHRASE}\n`, { mode: 0o600 });
    const r = spawnSync(this.bins.ctl, ["--dir", dir, "init", "--app", app, "--origin", origin], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      // vaultctl reads the passphrase from the environment (vault-dev from the file).
      env: { ...process.env, VAULTCTL_PASSPHRASE: PASSPHRASE },
    });
    if (r.error || r.status !== 0) {
      throw new Error(
        `vaultctl init failed (status ${r.status}, ${r.error ?? "no spawn error"}):\n${r.stderr}`,
      );
    }
    return { dir, passFile, app, origin };
  }

  /**
   * `vault-dev serve` for one pairing text; answer its prompt with `approve`.
   * `extraArgs` go after the others (e.g. `--ca-file ca.pem` for a wss relay).
   */
  serveVault(vault, pairText, extraArgs = []) {
    const n = this.procs.filter((p) => p.name.startsWith("vault-dev")).length + 1;
    const p = new Proc(`vault-dev #${n}`, this.bins.dev, [
      "serve",
      "--dir",
      vault.dir,
      "--pair",
      pairText,
      "--passphrase-file",
      vault.passFile,
      ...extraArgs,
    ]);
    this.procs.push(p);
    return new VaultDev(p);
  }
}

export class VaultDev {
  constructor(proc) {
    this.proc = proc;
  }

  /** The approval prompt: `{ n, code }` (`y <n>` approves it). */
  async prompt(ms = 30_000) {
    const m = await this.proc.waitFor(
      /\[#(\d+)\] Code: (\d+)[^\n]*\n\[#\1\] Type 'y \1' to allow/,
      ms,
      "the approval prompt",
    );
    return { n: Number(m[1]), code: m[2] };
  }

  answer(line) {
    this.proc.write(line);
  }

  hasPrompted() {
    return /Allow ".*" to open a vault session\?/.test(this.proc.text());
  }

  text() {
    return this.proc.text();
  }

  exit(ms = 30_000) {
    return this.proc.exit(ms);
  }
}

/** `vault-relay` on a free loopback port; `origin` is its `http://127.0.0.1:<port>`. */
export async function startRelay(bins) {
  const p = new Proc("vault-relay", bins.relay, [
    "--listen",
    "127.0.0.1:0",
    "--allow-origin",
    TEST_ORIGIN,
  ], { env: { RUST_LOG: "info" } });
  const m = await p.waitFor(/vault-relay listening[^\n]*?addr=(127\.0\.0\.1:\d+)/, 20_000, "the relay to listen");
  return { proc: p, origin: `http://${m[1]}`, stop: () => p.kill(), text: () => p.text() };
}

/** Records what the client reports through `onOffer`, `onSas` and `onState`. */
export class Probe {
  constructor() {
    this.events = [];
    this.listeners = new Set();
  }

  push(kind, value) {
    this.events.push({ kind, value });
    for (const l of this.listeners) l();
  }

  /** The callbacks for `pair`. */
  callbacks() {
    return {
      onOffer: (offer) => this.push("offer", offer),
      onSas: (sas) => this.push("sas", sas),
      onState: (state, reason, detail) => this.push("state", { state, reason, detail }),
    };
  }

  /** The first event (also already recorded) with `pred(event)`. */
  waitFor(pred, ms, what) {
    return withTimeout(
      new Promise((resolveEvent) => {
        const check = () => {
          const e = this.events.find(pred);
          if (e) {
            this.listeners.delete(check);
            resolveEvent(e.value);
          }
        };
        this.listeners.add(check);
        check();
      }),
      ms,
      `${what}; client events so far: ${this.describe()}`,
    );
  }

  offer(ms = 15_000) {
    return this.waitFor((e) => e.kind === "offer", ms, "onOffer");
  }

  sas(ms = 30_000) {
    return this.waitFor((e) => e.kind === "sas", ms, "onSas");
  }

  state(name, ms = 30_000) {
    return this.waitFor((e) => e.kind === "state" && e.value.state === name, ms, `state ${name}`);
  }

  states() {
    return this.events.filter((e) => e.kind === "state").map((e) => e.value);
  }

  describe() {
    return JSON.stringify(
      this.events.map((e) =>
        e.kind === "offer" ? { offer: `${e.value.text.slice(0, 24)}...` } : { [e.kind]: e.value },
      ),
    );
  }
}

/** The QR test vector shared with the Rust unit test (`vault-web/tests/qr-vector.txt`). */
export function qrVector() {
  const lines = readFileSync(join(repoRoot, "vault-web/tests/qr-vector.txt"), "utf8")
    .split("\n")
    .filter((l) => l !== "" && !l.startsWith("//"));
  const text = lines[0].replace(/^text: /, "");
  const size = Number(lines[1].replace(/^size: /, ""));
  return { text, size, rows: lines.slice(2) };
}

/** Runs `body(h)`; on failure, appends every process's output to the error. */
export async function withHarness(ctx, body) {
  const h = new Harness(ctx.bins, ctx.relay);
  try {
    await body(h);
  } catch (e) {
    const report = `\n===== process output for this test =====\n${h.dump()}\n===== end =====`;
    console.error(report);
    if (e instanceof Error) e.message += report;
    throw e;
  } finally {
    h.cleanup();
  }
}
