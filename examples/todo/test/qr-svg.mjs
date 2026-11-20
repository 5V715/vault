// SPDX-License-Identifier: MIT
// Reads the QR module rows back from the SVG renderQr draws (src/view.ts): a
// viewBox of size + 2 * 4 quiet modules and one path of "M{x} {y}h1v1h-1z" squares.

const QUIET = 4;

/** `#`/`.` rows (no quiet zone) from the svg's viewBox and path `d`. */
export function qrRowsFromSvg(viewBox, d) {
  const vb = /^0 0 (\d+) \1$/.exec(viewBox ?? "");
  if (!vb) throw new Error(`unexpected QR viewBox ${JSON.stringify(viewBox)}`);
  const size = Number(vb[1]) - 2 * QUIET;
  if (size < 21) throw new Error(`QR viewBox too small: ${viewBox}`);
  const grid = Array.from({ length: size }, () => Array(size).fill("."));
  const squares = (d ?? "").match(/M\d+ \d+h1v1h-1z/g) ?? [];
  if (squares.join("") !== d) throw new Error("unexpected QR path: not only 1x1 squares");
  for (const s of squares) {
    const [, xs, ys] = /^M(\d+) (\d+)/.exec(s);
    const x = Number(xs) - QUIET;
    const y = Number(ys) - QUIET;
    if (x < 0 || y < 0 || x >= size || y >= size) throw new Error(`QR module (${xs}, ${ys}) outside the code`);
    grid[y][x] = "#";
  }
  return grid.map((r) => r.join(""));
}
