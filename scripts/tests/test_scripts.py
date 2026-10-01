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


if __name__ == "__main__":
    unittest.main()
