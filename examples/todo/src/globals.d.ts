// SPDX-License-Identifier: MIT
// Replaced at build time by build.mjs (esbuild `define`); the unit-test build
// defines `__DEV__` as true so the dev-only view code can be tested.
declare const __RELAY_ORIGIN__: string;
/** `--runtime-config` build (the image): the relay comes from /config.json; `__RELAY_ORIGIN__` is "". */
declare const __RUNTIME_CONFIG__: boolean;
/** `--dev` build: the "Copy pairing text" button and its warning exist only then. */
declare const __DEV__: boolean;
