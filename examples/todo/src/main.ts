// SPDX-License-Identifier: MIT
/**
 * The page's entry: load the vault-web wasm, then hand the screens to `TodoApp`.
 * build.mjs defines `__RELAY_ORIGIN__` (from `--relay`) and `__DEV__` (`--dev`).
 * The wasm is bundled as bytes, so loading it needs no fetch (the CSP's
 * connect-src lists the relay only).
 */

import { loadWeb, pair } from "vault-web";
import * as glue from "vault-web/web";
import wasmBytes from "vault-web/web/vault_web_bg.wasm";

import { TodoApp } from "./app.js";
import { createUi } from "./view.js";

declare const __RELAY_ORIGIN__: string;
declare const __DEV__: boolean;

async function main(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) throw new Error("vault-todo: #app missing");
  const ui = createUi(document, root, { dev: __DEV__ });
  try {
    await loadWeb(glue, wasmBytes);
  } catch (e) {
    console.error("vault-todo: loading vault-web failed:", e);
    ui.showClosed("Could not load the vault module.", { onPairAgain: () => location.reload() });
    return;
  }
  const app = new TodoApp({
    pair: (opts) => pair(opts),
    ui,
    relayOrigin: __RELAY_ORIGIN__,
    deviceName: "Vault to-do (browser)",
    dev: __DEV__,
    // Development builds only: vault-dev takes the pairing text on its command line.
    ...(__DEV__ ? { copyText: (text: string) => navigator.clipboard.writeText(text) } : {}),
  });
  app.start();
}

void main();
