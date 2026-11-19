#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Builds the release archive for one target.

    package.py --version 1.2.3 --target x86_64-unknown-linux-musl \
               --bin-dir target/x86_64-unknown-linux-musl/release --out dist

Writes `vault-<version>-<target>.tar.gz` (`.zip` for Windows targets) holding
`vaultctl`, `vault-dev`, `vault-relay` (`.exe` on Windows), `LICENSE` and `README.md`
at the top level. The output is deterministic for identical inputs: entries are sorted,
owners are root, modes are fixed (0755 binaries, 0644 other files), the gzip header
carries no name or time, and every mtime is SOURCE_DATE_EPOCH (default: 0). The same
script runs on every runner, so Linux, macOS and Windows archives are built alike.
"""
import argparse
import gzip
import io
import os
import sys
import tarfile
import time
import zipfile
from pathlib import Path

BINARIES = ("vaultctl", "vault-dev", "vault-relay")
DOCS = ("LICENSE", "README.md")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True, help="tag without the leading v")
    ap.add_argument("--target", required=True)
    ap.add_argument("--bin-dir", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    a = ap.parse_args()

    windows = "windows" in a.target
    repo = Path(__file__).resolve().parent.parent
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))

    # (name in archive, source path, mode), sorted by archive name.
    entries = []
    for b in BINARIES:
        name = b + (".exe" if windows else "")
        entries.append((name, a.bin_dir / name, 0o755))
    for d in DOCS:
        entries.append((d, repo / d, 0o644))
    entries.sort(key=lambda e: e[0])
    for _, src, _ in entries:
        if not src.is_file():
            print(f"package.py: missing {src}", file=sys.stderr)
            return 1

    a.out.mkdir(parents=True, exist_ok=True)
    base = f"vault-{a.version}-{a.target}"
    if windows:
        path = a.out / f"{base}.zip"
        date_time = time.gmtime(max(epoch, 315532800))[:6]  # zip cannot go before 1980
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
            for name, src, mode in entries:
                zi = zipfile.ZipInfo(name, date_time)
                zi.compress_type = zipfile.ZIP_DEFLATED
                zi.create_system = 3
                zi.external_attr = (0o100000 | mode) << 16
                z.writestr(zi, src.read_bytes())
    else:
        path = a.out / f"{base}.tar.gz"
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as t:
            for name, src, mode in entries:
                data = src.read_bytes()
                ti = tarfile.TarInfo(name)
                ti.size = len(data)
                ti.mode = mode
                ti.mtime = epoch
                ti.uid = ti.gid = 0
                ti.uname = ti.gname = ""
                t.addfile(ti, io.BytesIO(data))
        with open(path, "wb") as f:
            with gzip.GzipFile(filename="", mode="wb", fileobj=f, mtime=0, compresslevel=9) as g:
                g.write(raw.getvalue())
    print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
