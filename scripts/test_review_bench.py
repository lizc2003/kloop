import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
SPEC = importlib.util.spec_from_file_location("review_bench", pathlib.Path(__file__).with_name("review-bench.py"))
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


class PreloadTest(unittest.TestCase):
    def test_preload_carries_whole_changed_functions_and_other_files_briefly(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = pathlib.Path(temporary)

            def git(*args):
                subprocess.run(["git", "-C", str(repo), "-c", "user.name=F", "-c", "user.email=f@example.invalid",
                                "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
                               check=True, capture_output=True)

            git("init", "-qb", "main")
            body = "\n".join(f"    let v{n} = {n};" for n in range(30))
            (repo / "lib.rs").write_text(f"fn target() {{\n{body}\n    let changed = 0;\n}}\n// spacer\n// spacer\n// spacer\n// spacer\n\nfn other() {{}}\n")
            (repo / "notes.md").write_text("\n".join(f"line {n}" for n in range(40)) + "\n")
            git("add", "-A")
            git("commit", "-qm", "base")
            (repo / "lib.rs").write_text(f"fn target() {{\n{body}\n    let changed = 1;\n}}\n// spacer\n// spacer\n// spacer\n// spacer\n\nfn other() {{}}\n")
            (repo / "notes.md").write_text("\n".join(f"line {n}" for n in range(39)) + "\nedited\n")
            git("commit", "-qam", "change")
            (repo / ".git/info/attributes").write_text("*.rs diff=rust\n")
            text = bench.preload(repo)
        self.assertIn("## The change, preloaded", text)
        self.assertIn("lib.rs", text)
        # 改动在函数末尾，-W 仍带上整个函数，从函数头开始。
        self.assertIn(" fn target() {", text)
        self.assertIn("     let v0 = 0;", text)
        self.assertIn("+    let changed = 1;", text)
        # 默认 3 行上下文之外的无关函数不带。
        self.assertNotIn("fn other", text)
        # 非代码文件只带 3 行上下文。
        self.assertIn("+edited", text)
        self.assertNotIn(" line 30", text)


if __name__ == "__main__":
    unittest.main()
