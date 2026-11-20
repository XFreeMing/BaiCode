import os
from pathlib import Path
import subprocess
import tempfile
import unittest

INSTALL = Path(__file__).with_name("install.sh")


class LocalInstallTest(unittest.TestCase):
    def test_install_and_update_leave_codex_untouched(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dest = root / ".local/bin"
            dest.mkdir(parents=True)
            codex = dest / "codex"
            codex.write_text("#!/bin/sh\necho 'codex original'\n")
            codex.chmod(0o755)
            (dest / "baic").symlink_to(codex)
            env = {"HOME": tmp, "PATH": os.defpath}
            for version in ("one", "two"):
                source = root / "candidate"
                source.write_text(f"#!/bin/sh\necho 'baic {version}'\n")
                source.chmod(0o755)
                subprocess.run(["sh", INSTALL, source], env=env, check=True)
                self.assertEqual(subprocess.check_output([dest / "baic"], text=True).strip(), f"baic {version}")
            self.assertNotEqual(subprocess.run(["sh", INSTALL, codex], env=env).returncode, 0)
            self.assertEqual(subprocess.check_output([codex], text=True).strip(), "codex original")
            self.assertFalse((root / ".codex").exists())
