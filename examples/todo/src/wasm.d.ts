// SPDX-License-Identifier: MIT
// build.mjs bundles .wasm files as bytes (esbuild `binary` loader): the page's
// CSP has no `connect-src 'self'`, so the wasm is not fetched.
declare module "*.wasm" {
  const bytes: Uint8Array<ArrayBuffer>;
  export default bytes;
}
