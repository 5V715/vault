// SPDX-License-Identifier: MIT
// qrRowsFromSvg (used by the image-mode browser e2e to read the pairing QR the page
// drew) is the exact inverse of renderQr, checked on the shared QR test vector.

import assert from "node:assert/strict";
import { test } from "node:test";

import { qrVector } from "../../../vault-web/js/test/helpers.mjs";
import { renderQr } from "../.test-build/view.js";
import { ShimDocument } from "./dom-shim.mjs";
import { qrRowsFromSvg } from "./qr-svg.mjs";

test("qrRowsFromSvg inverts renderQr", () => {
  const v = qrVector();
  const modules = new Uint8Array(v.rows.join("").split("").map((c) => (c === "#" ? 1 : 0)));
  const svg = renderQr(new ShimDocument(), { size: v.size, modules });
  const rows = qrRowsFromSvg(svg.getAttribute("viewBox"), svg.all("path")[0].getAttribute("d"));
  assert.deepEqual(rows, v.rows);
});

test("qrRowsFromSvg refuses what renderQr does not draw", () => {
  assert.throws(() => qrRowsFromSvg("0 0 11", "M4 4h1v1h-1z"), /viewBox/);
  assert.throws(() => qrRowsFromSvg("0 0 11 11", ""), /too small/);
  assert.throws(() => qrRowsFromSvg("0 0 29 29", "M4 4h2v1h-2z"), /path/);
  assert.throws(() => qrRowsFromSvg("0 0 29 29", "M1 1h1v1h-1z"), /outside/);
});
