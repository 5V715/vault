// SPDX-License-Identifier: MIT
// The app's sources never use an HTML-parsing DOM sink: text goes in with
// textContent, elements with createElement(NS). (CI greps vault-web/js too.)

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const src = join(dirname(fileURLToPath(import.meta.url)), "..", "src");
const SINKS = /\b(innerHTML|outerHTML|insertAdjacentHTML|document\.write(ln)?|createContextualFragment|DOMParser|srcdoc|setHTMLUnsafe|parseHTMLUnsafe)\b|\beval\s*\(|new\s+Function\s*\(/;

test("no_html_sinks_in_src", () => {
  const files = readdirSync(src, { recursive: true }).filter((f) => /\.(ts|js|mjs)$/.test(f));
  assert.ok(files.length >= 4, `sources found: ${files}`);
  const hits = [];
  for (const f of files) {
    readFileSync(join(src, f), "utf8")
      .split("\n")
      .forEach((line, i) => {
        if (SINKS.test(line)) hits.push(`src/${f}:${i + 1}: ${line.trim()}`);
      });
  }
  assert.deepEqual(hits, []);
});
