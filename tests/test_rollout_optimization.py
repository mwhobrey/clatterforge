import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/pilot3-concurrency.py"
MESSAGE = "Pilot 3 requires Python assertions; do not use -O or -OO."

class OptimizationContract(unittest.TestCase):
    def invoke(self, flags, args=(), optimize_env=None):
        env = os.environ.copy()
        env.pop("PYTHONOPTIMIZE", None)
        if optimize_env is not None:
            env["PYTHONOPTIMIZE"] = optimize_env
        return subprocess.run([sys.executable, "-B", *flags, str(SCRIPT), *args],
                              capture_output=True, text=True, timeout=10, env=env)

    def assert_rejected(self, result):
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(), MESSAGE)
        self.assertEqual(result.stdout, "")

    def test_optimization_flags_rejected_before_argument_access(self):
        for flag in ("-O", "-OO"):
            with self.subTest(flag=flag):
                self.assert_rejected(self.invoke([flag]))

    def test_environment_optimization_rejected_without_creating_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "evidence-must-not-exist"
            self.assert_rejected(self.invoke([], [str(Path(tmp) / "missing-binary.exe"), str(target)], "1"))
            self.assertFalse(target.exists())

    def test_normal_python_does_not_report_optimization_rejection(self):
        result = self.invoke([])
        self.assertNotIn(MESSAGE, result.stderr)
        self.assertNotEqual(result.returncode, 0)

if __name__ == "__main__":
    unittest.main()
