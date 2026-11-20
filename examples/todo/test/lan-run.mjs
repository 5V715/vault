// SPDX-License-Identifier: MIT
// Runs test/lan.test.mjs (LAN mode e2e) with a throwaway local CA: makes the CA and
// certificates for 127.0.0.1 and 127.0.0.2 with scripts/lan-cert.sh (the code path
// of scripts/showcase.sh --lan), then starts the test with NODE_EXTRA_CA_CERTS
// pointing at the CA (Node reads it only at process start). Removes it all after.

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const script = resolve(here, "../../../scripts/lan-cert.sh");
const base = mkdtempSync(join(tmpdir(), "todo-lan-ca-"));
const caDir = join(base, "ca");
let status = 1;
try {
  for (const ip of ["127.0.0.1", "127.0.0.2"]) {
    const r = spawnSync("bash", [script, "--dir", caDir, "--ip", ip], { stdio: "inherit" });
    if (r.status !== 0) throw new Error(`lan-cert.sh --ip ${ip} failed (status ${r.status}, ${r.error ?? "no spawn error"})`);
  }
  const r = spawnSync(
    process.execPath,
    ["--test", "--test-force-exit", "--test-concurrency=1", "--test-reporter=spec", join(here, "lan.test.mjs")],
    {
      stdio: "inherit",
      env: { ...process.env, NODE_EXTRA_CA_CERTS: join(caDir, "ca.pem"), TODO_LAN_CA_DIR: caDir },
    },
  );
  status = r.status ?? 1;
} catch (e) {
  console.error(`lan-run: ${e.message}`);
} finally {
  rmSync(base, { recursive: true, force: true });
}
process.exit(status);
