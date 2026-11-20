// SPDX-License-Identifier: MIT
// Runs test/lan.test.mjs (LAN mode e2e) with a throwaway local CA: makes the CA and
// the certificate for 127.0.0.1 with scripts/lan-cert.sh (the code path of
// scripts/showcase.sh --lan). For lan_wrong_ip_cert_refused it also signs a
// certificate for 127.0.0.2 with the same CA key directly (lan-cert.sh would make a
// new CA, since the CA is name-constrained to 127.0.0.1/32). Then it starts the test
// with NODE_EXTRA_CA_CERTS pointing at the CA (Node reads it only at process start).
// Removes it all after.

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const script = resolve(here, "../../../scripts/lan-cert.sh");
const base = mkdtempSync(join(tmpdir(), "todo-lan-ca-"));
const caDir = join(base, "ca");
const wrongDir = join(base, "wrong");
let status = 1;

function openssl(...args) {
  const r = spawnSync("openssl", args, { encoding: "utf8" });
  if (r.status !== 0) throw new Error(`openssl ${args[0]} failed (status ${r.status}): ${r.error ?? r.stderr}`);
}

try {
  const r0 = spawnSync("bash", [script, "--dir", caDir, "--ip", "127.0.0.1"], { stdio: "inherit" });
  if (r0.status !== 0) throw new Error(`lan-cert.sh failed (status ${r0.status}, ${r0.error ?? "no spawn error"})`);
  // A leaf for 127.0.0.2 from the same CA, with the extensions lan-cert.sh uses.
  mkdirSync(wrongDir, { mode: 0o700 });
  const key = join(wrongDir, "leaf-127.0.0.2.key");
  const cnf = join(wrongDir, "leaf.cnf");
  writeFileSync(
    cnf,
    "basicConstraints = critical, CA:false\nkeyUsage = critical, digitalSignature\n" +
      "extendedKeyUsage = serverAuth\nsubjectAltName = IP:127.0.0.2, DNS:localhost\n",
  );
  openssl("ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", key);
  openssl("req", "-new", "-key", key, "-subj", "/CN=vault showcase server", "-out", join(wrongDir, "leaf.csr"));
  openssl(
    "x509", "-req", "-in", join(wrongDir, "leaf.csr"), "-CA", join(caDir, "ca.pem"), "-CAkey", join(caDir, "ca.key"),
    "-set_serial", "0x1d", "-days", "1", "-sha256", "-extfile", cnf, "-out", join(wrongDir, "leaf-127.0.0.2.pem"),
  );
  const r = spawnSync(
    process.execPath,
    ["--test", "--test-force-exit", "--test-concurrency=1", "--test-reporter=spec", join(here, "lan.test.mjs")],
    {
      stdio: "inherit",
      env: {
        ...process.env,
        NODE_EXTRA_CA_CERTS: join(caDir, "ca.pem"),
        TODO_LAN_CA_DIR: caDir,
        TODO_LAN_WRONG_DIR: wrongDir,
      },
    },
  );
  status = r.status ?? 1;
} catch (e) {
  console.error(`lan-run: ${e.message}`);
} finally {
  rmSync(base, { recursive: true, force: true });
}
process.exit(status);
