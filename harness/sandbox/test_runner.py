import importlib.util
import pathlib
import unittest


def load_runner_module():
    runner_path = pathlib.Path(__file__).with_name("runner.py")
    spec = importlib.util.spec_from_file_location("runner", runner_path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


runner = load_runner_module()


class RunnerTests(unittest.TestCase):
    def test_allows_safe_modules(self):
        for mod in ("json", "math", "re"):
            self.assertIsNotNone(runner._restricted_import(mod))

    def test_blocks_unsafe_modules(self):
        for mod in ("os", "subprocess", "sys"):
            with self.assertRaises(ImportError):
                runner._restricted_import(mod)

    def test_blocks_private_attribute_access(self):
        with self.assertRaises(runner.SandboxViolation):
            runner._validate_and_compile("print(tool.__class__)")

    def test_safe_builtins_exclude_open(self):
        self.assertNotIn("open", runner._SAFE_BUILTINS)

    def test_bounded_capture_truncates_large_output(self):
        capture = runner._BoundedCapture(16)
        capture.write("x" * 32)
        self.assertIn("output truncated", capture.getvalue())


if __name__ == "__main__":
    unittest.main()
