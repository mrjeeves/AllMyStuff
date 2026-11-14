#!/usr/bin/env python3
"""Audit the resolved SMB dependency closure and preserve its upstream notices.

Input: cargo metadata --locked --manifest-path node/Cargo.toml --format-version 1
The application remains MIT. Dependency notices retain their own licenses.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys

# Closed expressions deliberately require review when a dependency changes its
# declared terms. Select MIT where offered; retain the BSD/Unicode notices too.
PERMISSIVE = {
    "MIT",
    "MIT OR Apache-2.0",
    "Apache-2.0 OR MIT",
    "MIT/Apache-2.0",
    "Zlib OR Apache-2.0 OR MIT",
    "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT",
    "(MIT OR Apache-2.0) AND Unicode-3.0",
    "BSD-3-Clause",
    "Zlib",
    "Unlicense OR MIT",
    "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
    "Apache-2.0 WITH LLVM-exception",
}


def audit(metadata):
    packages = {p["id"]: p for p in metadata["packages"]}
    graph = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    roots = [p["id"] for p in packages.values() if p["name"] == "smb-server"]
    if len(roots) != 1:
        raise ValueError("expected exactly one pinned smb-server dependency")
    seen = set()
    pending = roots[:]
    while pending:
        package_id = pending.pop()
        if package_id in seen:
            continue
        seen.add(package_id)
        pending.extend(graph[package_id]["dependencies"])
    result = sorted((packages[p] for p in seen), key=lambda p: (p["name"], p["version"]))
    for package in result:
        if package.get("license") not in PERMISSIVE:
            raise ValueError(f"review required: {package['name']} {package['version']}: {package.get('license')}")
    return result


def notices(packages):
    sections = [
        "AllMyStuff SMB dependency notices\n\n"
        "AllMyStuff's integration code is MIT licensed. These dependencies retain\n"
        "their upstream licenses and notices; this file does not relicense them.\n"
        "MIT is selected where available. BSD, Unicode and Zlib notices also apply.\n"
        "Native Windows/macOS/Linux filesystem clients are supplied by the OS,\n"
        "and their implementations are not bundled or copied into AMS.\n"
    ]
    license_texts = {}
    for package in packages:
        root = Path(package["manifest_path"]).parent
        files = sorted(p for p in root.iterdir() if p.is_file() and p.name.lower().startswith(("license", "licence", "copying", "copyright", "notice")))
        if not files:
            # binrw's published workspace members omit the repository LICENSE.
            # Preserve the license from their exact .cargo_vcs_info commit.
            fallback = Path(__file__).resolve().parents[1] / "docs/smb-upstream-licenses/binrw.txt"
            if package["name"] in {"binrw", "binrw_derive"} and package["version"] == "0.15.2":
                vcs = json.loads((root / ".cargo_vcs_info.json").read_text())["git"]["sha1"]
                if f"/{vcs}/LICENSE" not in fallback.read_text(encoding="utf-8"):
                    raise ValueError("binrw notice does not match its published source commit")
                files = [fallback]
            else:
                raise ValueError(f"upstream notice missing: {package['name']} {package['version']}")
        heading = f"\n{'=' * 72}\n{package['name']} {package['version']}\nSPDX: {package['license']}\n"
        references = []
        for path in files:
            content = path.read_text(encoding="utf-8").rstrip() + "\n"
            identifier = hashlib.sha256(content.encode("utf-8")).hexdigest()
            license_texts[identifier] = content
            references.append(f"{path.name}: {identifier}\n")
        sections.append(heading + "".join(references))
    for identifier, content in sorted(license_texts.items()):
        sections.append(f"\n{'=' * 72}\nLicense text {identifier}\n\n{content}")
    return "".join(sections)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata-extra", type=Path, action="append", default=[])
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--write-notices", type=Path)
    group.add_argument("--check-notices", type=Path)
    args = parser.parse_args()
    try:
        packages = audit(json.load(sys.stdin))
        by_id = {p["id"]: p for p in packages}
        for path in args.metadata_extra:
            with path.open(encoding="utf-8-sig") as stream:
                by_id.update((p["id"], p) for p in audit(json.load(stream)))
        packages = sorted(by_id.values(), key=lambda p: (p["name"], p["version"]))
        if args.write_notices or args.check_notices:
            text = notices(packages)
            if args.write_notices:
                args.write_notices.parent.mkdir(parents=True, exist_ok=True)
                args.write_notices.write_text(text, encoding="utf-8", newline="\n")
            else:
                committed = args.check_notices.read_text(encoding="utf-8")
                # One committed notice covers the union of all desktop targets;
                # each native CI job checks its own resolved platform subset.
                if any(section not in committed for section in text.split("\n" + "=" * 72 + "\n")[1:]):
                    raise ValueError("SMB notices differ from the locked dependency graph; regenerate and review them")
        print(f"SMB license audit passed: {len(packages)} dependencies; permissive licenses and upstream notices retained")
    except (KeyError, ValueError, OSError) as error:
        print(f"SMB license audit failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
