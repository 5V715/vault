#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Test-only stand-in for the GitHub CLI. Never used outside the install/sign tests.

Serves releases from a local directory instead of GitHub:

    $VAULT_FAKE_GH_DIR/latest              tag of the latest published release
    $VAULT_FAKE_GH_DIR/releases/<tag>/*    the release's assets
    $VAULT_FAKE_GH_DIR/releases/<tag>/.draft   present while the release is a draft
    $VAULT_FAKE_GH_DIR/releases/<tag>/.prerelease   present if marked as a pre-release
    $VAULT_FAKE_GH_DIR/order               optional: tags newest first (default: name, descending)

Environment:
    VAULT_FAKE_GH_DIR         the directory above (required)
    VAULT_FAKE_GH_AUTH_FAIL   "1": `gh auth status` fails as if not logged in
    VAULT_FAKE_GH_LOG         every invocation is appended here (one line, args joined by a tab)

Supported: auth status; release view [tag] --json F --jq EXPR (EXPR: .tagName, .isDraft,
.assets[].name); release list [--exclude-drafts] [--exclude-pre-releases] [--limit N]
--json F --jq '.[] | [.tagName, (.isLatest|tostring)] | @tsv'; release download <tag> -p NAME... -D DIR; release upload <tag> [--clobber] FILE...;
release edit <tag> --draft=false. Every command must name --repo 5V715/vault (or -R).
"""
import fnmatch
import json
import os
import shutil
import sys
from pathlib import Path

REPO = "5V715/vault"


def die(msg, code=1):
    print(msg, file=sys.stderr)
    sys.exit(code)


def take(args, *names):
    """Removes every `name VALUE` / `name=VALUE` from args; returns the values."""
    out, rest, i = [], [], 0
    while i < len(args):
        a = args[i]
        hit = next((n for n in names if a == n or a.startswith(n + "=")), None)
        if hit is None:
            rest.append(a)
            i += 1
        elif a == hit:
            if i + 1 >= len(args):
                die(f"flag needs an argument: {a}")
            out.append(args[i + 1])
            i += 2
        else:
            out.append(a.split("=", 1)[1])
            i += 1
    args[:] = rest
    return out


def main():
    argv = sys.argv[1:]
    log = os.environ.get("VAULT_FAKE_GH_LOG")
    if log:
        with open(log, "a", encoding="utf-8") as f:
            f.write("\t".join(argv) + "\n")
    root = Path(os.environ["VAULT_FAKE_GH_DIR"])
    releases = root / "releases"

    if argv[:2] == ["auth", "status"]:
        if os.environ.get("VAULT_FAKE_GH_AUTH_FAIL") == "1":
            die("You are not logged into any GitHub hosts. To log in, run: gh auth login")
        print("Logged in to github.com account test (fake)")
        return
    if argv[:1] != ["release"] or len(argv) < 2:
        die(f"fake gh: unsupported command: {argv}")
    sub, args = argv[1], argv[2:]
    repos = take(args, "--repo", "-R")
    if repos != [REPO]:
        die(f"fake gh: expected --repo {REPO}, got {repos}")

    def release_dir(tag):
        d = releases / tag
        if not d.is_dir():
            die("release not found")
        return d

    if sub == "list":
        exclude_drafts = "--exclude-drafts" in args
        exclude_pre = "--exclude-pre-releases" in args
        args[:] = [a for a in args if a not in ("--exclude-drafts", "--exclude-pre-releases")]
        limit = take(args, "--limit", "-L")
        fields = take(args, "--json")
        jq = take(args, "--jq", "-q")
        if args or not fields or jq != ['.[] | [.tagName, (.isLatest|tostring)] | @tsv']:
            die(f"fake gh: unsupported list arguments: {argv}")
        order = root / "order"
        if order.is_file():
            tags = [t for t in order.read_text().split() if (releases / t).is_dir()]
        else:
            tags = sorted((p.name for p in releases.iterdir() if p.is_dir()), reverse=True)
        latest = (root / "latest").read_text().strip() if (root / "latest").is_file() else None
        shown = 0
        for t in tags:
            if exclude_drafts and (releases / t / ".draft").exists():
                continue
            if exclude_pre and (releases / t / ".prerelease").exists():
                continue
            if limit and shown >= int(limit[0]):
                break
            print(f"{t}\t{'true' if t == latest else 'false'}")
            shown += 1
        return
    if sub == "view":
        fields = take(args, "--json")
        jq = take(args, "--jq", "-q")
        if args:
            tag = args[0]
        else:
            latest = root / "latest"
            if not latest.is_file():
                die("release not found")
            tag = latest.read_text().strip()
            if jq == [".tagName"]:
                # Whatever the "server" calls latest, even a malformed tag.
                print(tag)
                return
        d = release_dir(tag)
        data = {
            "tagName": tag,
            "isDraft": (d / ".draft").exists(),
            "assets": [{"name": p.name} for p in sorted(d.iterdir()) if not p.name.startswith(".")],
        }
        if not fields or len(jq) != 1:
            die("fake gh: release view needs --json and --jq")
        expr = jq[0]
        if expr == ".tagName":
            print(data["tagName"])
        elif expr == ".isDraft":
            print("true" if data["isDraft"] else "false")
        elif expr == ".assets[].name":
            for a in data["assets"]:
                print(a["name"])
        else:
            die(f"fake gh: unsupported --jq {expr}")
    elif sub == "download":
        patterns = take(args, "--pattern", "-p")
        dirs = take(args, "--dir", "-D")
        if len(args) != 1 or len(dirs) != 1 or not patterns:
            die(f"fake gh: unsupported download arguments: {argv}")
        d = release_dir(args[0])
        dest = Path(dirs[0])
        hits = [p for p in sorted(d.iterdir()) if not p.name.startswith(".")
                and any(fnmatch.fnmatchcase(p.name, pat) for pat in patterns)]
        if not hits:
            die("no assets match the file pattern")
        dest.mkdir(parents=True, exist_ok=True)
        for p in hits:
            if (dest / p.name).exists():
                die(f"{dest / p.name} already exists (use `--clobber` to overwrite file)")
            shutil.copyfile(p, dest / p.name)
    elif sub == "upload":
        clobber = "--clobber" in args
        args[:] = [a for a in args if a != "--clobber"]
        if len(args) < 2:
            die("fake gh: upload needs a tag and files")
        d = release_dir(args[0])
        for f in args[1:]:
            if (d / Path(f).name).exists() and not clobber:
                die(f"asset {Path(f).name} already exists")
            shutil.copyfile(f, d / Path(f).name)
    elif sub == "edit":
        if args[1:] != ["--draft=false"]:
            die(f"fake gh: unsupported edit arguments: {argv}")
        d = release_dir(args[0])
        (d / ".draft").unlink(missing_ok=True)
    else:
        die(f"fake gh: unsupported release subcommand: {sub}")


if __name__ == "__main__":
    main()
