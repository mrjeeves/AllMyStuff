#!/usr/bin/env python3
"""Exercise the real release bump with four tiny, offline Cargo workspaces."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


BUMP_SCRIPT = Path(__file__).with_name("bump-version.sh")
BASH = os.environ.get("BASH", "bash")
MANIFESTS = (
    "Cargo.toml",
    "node/Cargo.toml",
    "gui/src-tauri/Cargo.toml",
    "gui/mobile/Cargo.toml",
)


class ReleaseBumpTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="allmystuff-version-bump-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write("scripts/bump-version.sh", BUMP_SCRIPT.read_text(encoding="utf-8"))
        self.write(
            "Cargo.toml",
            '[workspace]\nmembers = ["crates/graph"]\n'
            'exclude = ["node", "gui/src-tauri", "gui/mobile", "pixels"]\n'
            '[workspace.package]\nversion = "0.2.123"\n'
            '[workspace.dependencies]\n'
            'allmystuff-graph = { path = "crates/graph", version = "0.2.123" }\n',
        )
        self.write(
            "crates/graph/Cargo.toml",
            '[package]\nname = "allmystuff-graph"\nversion.workspace = true\n'
            'edition = "2021"\n',
        )
        self.write("crates/graph/src/lib.rs", "")
        self.write(
            "pixels/Cargo.toml",
            '[workspace]\n[package]\nname = "allmystuff-pixels"\n'
            'version = "0.1.0"\nedition = "2021"\n',
        )
        self.write("pixels/src/lib.rs", "")
        for directory, name, graph_path, node_path, pixels_path in (
            ("node", "node", "../crates/graph", None, "../pixels"),
            ("gui/src-tauri", "gui", "../../crates/graph", "../../node", None),
            ("gui/mobile", "mobile", "../../crates/graph", "../../node", None),
        ):
            manifest = (
                f'[workspace]\n[package]\nname = "allmystuff-{name}"\n'
                'version = "0.2.123"\nedition = "2021"\n[dependencies]\n'
                f'allmystuff-graph = {{ path = "{graph_path}" }}\n'
            )
            if node_path:
                manifest += f'allmystuff-node = {{ path = "{node_path}" }}\n'
            if pixels_path:
                manifest += f'allmystuff-pixels = {{ path = "{pixels_path}" }}\n'
            self.write(f"{directory}/Cargo.toml", manifest)
            self.write(f"{directory}/src/lib.rs", "")
        for relative in ("gui/package.json", "gui/mobile/package.json", "gui/mobile/tauri.conf.json"):
            self.write(relative, '{"version": "0.2.123"}\n')
        for manifest in MANIFESTS:
            self.cargo("generate-lockfile", "--offline", "--manifest-path", manifest)

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def cargo(self, *args):
        result = subprocess.run(
            ["cargo", *args], cwd=self.root, capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def bump(self, version):
        return subprocess.run(
            [BASH, "scripts/bump-version.sh", version],
            cwd=self.root, capture_output=True, text=True,
        )

    def test_bump_keeps_all_four_workspaces_locked(self):
        for version in ("0.2.124", "0.3.0-rc.1"):
            result = self.bump(version)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            for manifest in MANIFESTS:
                metadata = json.loads(self.cargo(
                    "metadata", "--locked", "--offline", "--format-version", "1",
                    "--manifest-path", manifest,
                ).stdout)
                packages = {package["name"]: package["version"] for package in metadata["packages"]}
                self.assertEqual(packages["allmystuff-graph"], version)
                if manifest != "Cargo.toml":
                    self.assertEqual(packages["allmystuff-node"], version)
                    self.assertEqual(packages["allmystuff-pixels"], "0.1.0")
                for name in ("allmystuff-gui", "allmystuff-mobile"):
                    if name in packages:
                        self.assertEqual(packages[name], version)
            for relative in ("gui/package.json", "gui/mobile/package.json", "gui/mobile/tauri.conf.json"):
                self.assertEqual(json.loads((self.root / relative).read_text())["version"], version)

    def test_root_resolution_failure_stops_release(self):
        (self.root / "crates/graph/Cargo.toml").unlink()
        result = self.bump("0.2.124")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("ok", result.stdout.splitlines())

    def test_subworkspace_resolution_failure_stops_release(self):
        with (self.root / "node/Cargo.toml").open("a", encoding="utf-8") as manifest:
            manifest.write('missing = { path = "../missing" }\n')
        result = self.bump("0.2.124")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("ok", result.stdout.splitlines())


if __name__ == "__main__":
    unittest.main()
