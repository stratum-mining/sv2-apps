#!/usr/bin/env python3
"""Run script regressions offline with: python3 -B -m unittest discover -s scripts/tests."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parents[1]


class ScriptTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}")

    def stub(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
        path.chmod(0o755)

    def run_script(self, name, *args, shell="sh", input=None):
        return subprocess.run(
            [shell, str(SCRIPTS / name), *args],
            cwd=self.root, env=self.env, input=input, text=True, capture_output=True,
        )


class ReleaseTests(ScriptTests):
    def setUp(self):
        super().setUp()
        self.log = self.root / "cargo.log"
        self.env.update(CARGO_LOG=str(self.log), CARGO_STATUS="0", CARGO_OUTPUT="")
        self.stub("cargo", 'printf "%s\\n" "$PWD" "$*" > "$CARGO_LOG"\n'
                  'echo "$CARGO_OUTPUT"\nexit "$CARGO_STATUS"')

    def test_failed_cd_never_runs_cargo(self):
        # A caller may itself be a publishable crate; publishing there is unsafe.
        manifest = self.root / "Cargo.toml"
        manifest.write_text('[package]\nname = "caller"\nversion = "1.0.0"\n')
        for path in [self.root / "missing", manifest]:
            with self.subTest(path=path):
                result = self.run_script("release-apps.sh", str(path))
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(self.log.exists(), "cargo ran after cd failed")

    def test_publish_outcomes_and_working_directory(self):
        crate = self.root / "crate with spaces"
        crate.mkdir()
        for status, output, expected in [
            (0, "published", 0),
            (101, "crate already exists", 0),
            (101, "network unavailable", 1),
        ]:
            with self.subTest(output=output):
                self.env.update(CARGO_STATUS=str(status), CARGO_OUTPUT=output)
                result = self.run_script("release-apps.sh", str(crate))
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual(self.log.read_text().splitlines(), [str(crate), "publish"])


class VersionBumpTests(ScriptTests):
    def setUp(self):
        super().setUp()
        self.crate = self.root / "crate"
        self.crate.mkdir()
        self.versions = self.root / "versions.json"
        self.env["VERSIONS_FILE"] = str(self.versions)
        self.stub("git", 'case "$1" in\n'
                  '  rev-parse) pwd ;;\n'
                  '  merge-base) echo base ;;\n'
                  '  diff) echo crate/README.md ;;\n'
                  '  *) exit 99 ;;\nesac')
        self.stub("curl", 'while [ "$#" -gt 0 ]; do\n'
                  '  if [ "$1" = "-o" ]; then cp "$VERSIONS_FILE" "$2"; break; fi\n'
                  '  shift\ndone\nprintf 200')
        self.stub("sleep", ":")

    def check_version(self, current, published, expected, latest=None):
        (self.crate / "Cargo.toml").write_text(
            f'[package]\nname = "example"\nversion = "{current}"\n'
        )
        self.versions.write_text(json.dumps({"versions": [
            {"num": version, "yanked": yanked} for version, yanked in published
        ]}))
        result = self.run_script("check-version-bumps.sh", "base")
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        if latest is not None:
            message = f"({latest})" if expected else f"{latest} -> {current}"
            self.assertIn(message, result.stdout)

    def test_semver_precedence(self):
        ordered = [
            "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta",
            "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0",
            "1.0.1", "1.1.0", "1.9.0", "1.10.0", "2.0.0",
        ]
        for lower, higher in zip(ordered, ordered[1:]):
            with self.subTest(lower=lower, higher=higher):
                self.check_version(higher, [(lower, False)], 0)
                self.check_version(lower, [(higher, False)], 1)
        self.check_version("1.0.0", [("1.0.0", False)], 1)

    def test_highest_published_uses_semver(self):
        versions = [("1.0.0", False), ("1.0.0-rc.10", False), ("1.0.0-rc.2", False)]
        self.check_version("1.0.0-rc.11", versions, 1, "1.0.0")
        self.check_version("1.0.1", versions, 0, "1.0.0")
        self.check_version("1.0.0-rc.11", versions[1:], 0, "1.0.0-rc.10")

    def test_build_metadata_does_not_increase_precedence(self):
        for version in ["1.0.0", "1.0.0-rc.1"]:
            with self.subTest(version=version):
                self.check_version(version + "+new", [(version + "+old", False)], 1)
                self.check_version(version + "+build", [(version, False)], 1)
                self.check_version(version, [(version + "+build", False)], 1)
        self.check_version("1.0.1", [("1.0.0+zzz", False), ("1.0.1+aaa", False)], 1)

    def test_yanked_versions_count(self):
        versions = [("1.0.0", False), ("2.0.0", True)]
        self.check_version("1.1.0", versions, 1, "2.0.0")
        self.check_version("2.0.0", versions, 1, "2.0.0")
        self.check_version("2.0.1", versions, 0, "2.0.0")

    def test_numeric_and_ascii_prerelease_identifiers(self):
        for lower, higher in [
            ("1.0.0-10", "1.0.0-1a"),
            ("1.0.0-A", "1.0.0-a"),
            ("1.0.0-a-b", "1.0.0-a-c"),
            ("1.0.0-rc.9007199254740992", "1.0.0-rc.9007199254740993"),
            ("9007199254740992.0.0", "9007199254740993.0.0"),
        ]:
            with self.subTest(lower=lower, higher=higher):
                self.check_version(higher, [(lower, False)], 0)
                self.check_version(lower, [(higher, False)], 1)


class CrossRepoTests(ScriptTests):
    def test_literal_home_and_absolute_paths(self):
        home = self.root / "home with spaces"
        self.env.update(HOME=str(home), CROSS_REPO_ROOT=str(self.root))
        for relative in ["stratum", "stratum with spaces"]:
            repo = home / relative
            (repo / "stratum-core").mkdir(parents=True)
            for path in [f"~/{relative}", str(repo)]:
                with self.subTest(path=path):
                    result = subprocess.run(
                        ["bash", "-c", 'source "$1"; get-stratum-core-path "$2"',
                         "bash", str(SCRIPTS / "cross-repo.sh"), path],
                        cwd=self.root, env=self.env, text=True, capture_output=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(result.stdout.strip(), str(repo / "stratum-core"))
                    self.assertEqual((self.root / "stratum").resolve(), repo)


if __name__ == "__main__":
    unittest.main()
