import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("coverage-summary.py")
SPEC = importlib.util.spec_from_file_location("coverage_summary", SCRIPT)
assert SPEC and SPEC.loader
coverage_summary = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = coverage_summary
SPEC.loader.exec_module(coverage_summary)


class CoverageSummaryTests(unittest.TestCase):
    def test_reports_core_and_orchestration_modules_but_not_adapters(self):
        report = """\
SF:/repo/src/net/protocol.rs
FNDA:1,decode
DA:1,1
DA:2,0
end_of_record
SF:/repo/src/app/update.rs
FNDA:0,execute
DA:4,1
DA:5,1
end_of_record
SF:/repo/src/input/inject.rs
DA:8,1
end_of_record
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lcov.info"
            path.write_text(report, encoding="utf-8")
            rendered = coverage_summary.render(coverage_summary.parse_lcov(path))

        self.assertIn("`src/net/protocol.rs`", rendered)
        self.assertIn("1/2 | 50.0%", rendered)
        self.assertIn("`src/app/update.rs`", rendered)
        self.assertNotIn("src/input/inject.rs", rendered)
        self.assertIn("| Core | 50.0% | 100.0% |", rendered)
        self.assertIn("| Orchestration | 100.0% | 0.0% |", rendered)

    def test_normalizes_relative_and_windows_source_paths(self):
        self.assertEqual(
            coverage_summary.repository_path(r"C:\repo\src\setup\flow.rs"),
            "src/setup/flow.rs",
        )
        self.assertEqual(
            coverage_summary.repository_path("./src/net/framing.rs"),
            "src/net/framing.rs",
        )


if __name__ == "__main__":
    unittest.main()
