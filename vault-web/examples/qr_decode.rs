// SPDX-License-Identifier: MIT
//! Test helper (examples/todo/test/image.test.mjs): decodes a QR module matrix
//! with an independent decoder (rqrr), as a phone camera would read the page.
//!
//!   qr_decode < rows.txt
//!
//! stdin: one line per module row, `#` dark and `.` light, no quiet zone.
//! stdout: the decoded text. Exit status 1 if it is not exactly one QR code.

use std::io::Read;

fn main() {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read stdin");
    let rows: Vec<&[u8]> = input
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::as_bytes)
        .collect();
    let size = rows.len();
    if size == 0 || rows.iter().any(|r| r.len() != size) {
        eprintln!("qr_decode: expected a square matrix of # and .");
        std::process::exit(1);
    }
    // Scale 4x with a 4-module light quiet zone, as the page draws it.
    let (scale, quiet) = (4, 4);
    let side = (size + 2 * quiet) * scale;
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| {
        let (mx, my) = (x / scale, y / scale);
        let dark = mx >= quiet
            && my >= quiet
            && mx < quiet + size
            && my < quiet + size
            && rows[my - quiet][mx - quiet] == b'#';
        if dark { 0 } else { 255 }
    });
    let grids = img.detect_grids();
    if grids.len() != 1 {
        eprintln!("qr_decode: found {} QR codes, expected 1", grids.len());
        std::process::exit(1);
    }
    match grids[0].decode() {
        Ok((_, text)) => println!("{text}"),
        Err(e) => {
            eprintln!("qr_decode: {e}");
            std::process::exit(1);
        }
    }
}
