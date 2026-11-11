import contextlib
import io
import json
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import review_pipeline as pipeline
import review_recheck as recheck
import review_rollout as rollout


def lines(count, changed=None, inserted_after=None):
    rows = []
    for number in range(1, count + 1):
        rows.append(f"line {number} changed" if number == changed else f"line {number}")
        if number == inserted_after:
            rows.append("inserted")
    return "\n".join(rows) + "\n"


class RolloutEvidenceTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name).resolve()
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-qb", "main")
        self.revisions = []
        for f_text in (lines(12), lines(12, changed=5, inserted_after=8)):
            (self.repo / "f.py").write_text(f_text)
            (self.repo / "g.py").write_text(lines(5))
            (self.repo / "h.py").write_text(lines(3))
            self.git("add", "-A")
            self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                     "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "Fixture")
            self.revisions.append(self.git("rev-parse", "HEAD").strip())
        self.versions = {"before": self.revisions[0], "after": self.revisions[1]}
        self.ledger = {"version": 1, "candidates": [
            {"id": "C1", "question": "Does g lose a guard?", "verdict": "excluded", "reason": "PRIVATE",
             "evidence": [{"version": "after", "path": "g.py", "line": 3}]},
            {"id": "C2", "question": "Is f wrong?", "verdict": "confirmed", "reason": "found",
             "evidence": [{"version": "after", "path": "f.py", "line": 5}]}]}

    def git(self, *args):
        return recheck.git(self.repo, *args)

    def write_rollout(self, path, calls):
        events = []
        for index, (name, arguments, output) in enumerate(calls):
            ident = f"t{index}"
            events.append({"type": "message", "role": "assistant", "content": [
                {"type": "tool_use", "id": ident, "name": name, "input": arguments}]})
            events.append({"type": "message", "role": "user", "content": [
                {"type": "tool_result", "tool_use_id": ident, "content": output}]})
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("".join(json.dumps(event) + "\n" for event in events))

    def fixture_calls(self):
        diff = self.git("diff", "-U0", "HEAD^", "HEAD", "--", "f.py")
        return [
            ("read_file", {"path": str(self.repo / "f.py"), "offset": 2, "limit": 2},
             "2\tline 2\n3\tline 3"),
            ("bash", {"command": "sed -n '6,7p' f.py"}, "line 6\nline 7\n"),
            ("bash", {"command": f"cd {self.repo} && git show HEAD^:f.py | sed -n 10,11p"}, "line 10\nline 11\n"),
            ("bash", {"command": "git diff -U0 HEAD^ HEAD -- f.py"}, diff),
            ("grep", {"path": str(self.repo), "pattern": "x", "output_mode": "content"}, "f.py:12:line 11"),
            ("grep", {"path": str(self.repo / "g.py"), "pattern": "x", "-n": True}, "3:line 3"),
            ("bash", {"command": "grep -n line g.py"}, "4:line 4\n"),
            ("bash", {"command": "cat h.py"}, lines(3)),
            # 以下都不应计入：仓库外文件、不存在的路径、越界行、不存在的版本文件、未知读法。
            ("read_file", {"path": "/etc/hosts"}, "1\tlocalhost"),
            ("bash", {"command": f"grep -rn x {self.repo}"}, f"{self.repo}/nope.py:3:x\n"),
            ("bash", {"command": "sed -n '50,60p' f.py"}, ""),
            ("bash", {"command": "git show HEAD:missing.py"}, "fatal"),
            ("bash", {"command": "python3 -c 'print(1)'"}, "1\n"),
        ]

    def test_only_reads_located_in_frozen_blobs_are_collected(self):
        path = self.root / "rollout.jsonl"
        self.write_rollout(path, self.fixture_calls())
        reads = rollout.read_regions(path, self.repo, self.repo, self.versions)
        # 只有来源明确的读取计入：read_file、grep 工具与绝对路径的 bash grep 输出；
        # sed、`git show | sed`、cat、bash 里的补丁、无路径或相对路径的 bash grep 不计。
        self.assertEqual(reads.lines, {("after", "f.py"): {2, 3, 12}, ("after", "g.py"): {3}})
        self.assertEqual(list(reads.first_seen), [("after", "f.py"), ("after", "g.py")])

    def test_bash_reads_count_only_with_an_explicit_source_and_line(self):
        notice = "\n[output truncated: 9000 characters and 0 additional bytes omitted]"
        diff = self.git("diff", "-U0", "HEAD^", "HEAD", "--", "f.py")
        pointer = ("[output truncated: 40000 chars, 900 lines; full output saved to /tmp/off-1.txt]\n"
                   + diff + "\n[Query it in place instead of reading it back: bash with `python3 -c '...'` "
                   "over that path, printing only the fields you need, so only what you extract enters the context]")
        calls = [
            # 来源要从命令推断的读取一律不计，不论写法多简单。
            ("bash", {"command": "sed -n '1,3p' f.py"}, "line 1\nline 2\nline 3\n"),
            ("bash", {"command": "sed -n '1,5p' g.py | head 3"}, "line 1\nline 2\nline 3\n"),
            ("bash", {"command": "nl -ba g.py | sed -n '4,5p'"}, "     4\tline 4\n     5\tline 5\n"),
            ("bash", {"command": "cat h.py"}, "line 1\nline 2\nline 3\n"),
            ("bash", {"command": "grep -n x|head g.py"}, "50: marker\n"),
            # grep 输出带绝对路径与行号：计入；截断说明之后的内容不算显示过。
            ("bash", {"command": f"grep -rn line {self.repo}"},
             f"{self.repo}/g.py:2:line 2\n" + notice + f"\n{self.repo}/g.py:5:line 5"),
            # 相对路径的基准取决于 cwd，无从确认：跳过，不按仓库根猜。
            ("bash", {"command": "cd src && rg -n --with-filename needle f.py"}, "f.py:2:line 2\n"),
            ("bash", {"command": "git grep -n line HEAD"}, "HEAD:g.py:3:line 3\n"),
            # 补丁路径的基准取决于 cwd、`git -C` 与 diff.relative 配置，命令里看不全：不计。
            ("bash", {"command": "git diff -U0 HEAD^ HEAD"}, diff),
            ("bash", {"command": "cd src && git -c diff.relative=true diff -U0 HEAD^ HEAD"}, diff),
            # 会话落盘预览：整条跳过。
            ("bash", {"command": f"grep -rn line {self.repo}"}, pointer.replace(diff, f"{self.repo}/g.py:4:line 4\n")),
            # 解析器假设之外的输入：这一条不计，不中断整次准备。
            ("read_file", {"path": None}, "1\tline 1"),
        ]
        path = self.root / "explicit.jsonl"
        self.write_rollout(path, calls)
        reads = rollout.read_regions(path, self.repo, self.repo, self.versions)
        self.assertEqual(reads.lines, {("after", "g.py"): {2}})
        self.assertEqual(reads.counts["relative-skipped"], 2)
        self.assertEqual(reads.counts["offloaded-skipped"], 1)
        self.assertEqual(reads.counts["unparsed"], 1)

    def test_spans_merge_gaps_of_at_most_three_lines(self):
        self.assertEqual(rollout.spans_of({1, 2, 3, 7, 8, 20}), [(1, 8), (20, 20)])
        self.assertEqual(rollout.spans_of({1, 6}), [(1, 1), (6, 6)])

    def test_packet_prefers_anchored_files_and_drops_whole_spans(self):
        path = self.root / "rollout.jsonl"
        self.write_rollout(path, self.fixture_calls())
        reads = rollout.read_regions(path, self.repo, self.repo, self.versions)
        full = rollout.rollout_packet(self.ledger, reads, self.repo, self.revisions[1], 160_000)
        self.assertEqual([c["id"] for c in full["candidates"]], ["C1"])
        self.assertNotIn("PRIVATE", full["prompt"])
        self.assertEqual(full["preparation"]["dropped"], [])
        # 锚点所在的 g.py 排在最前；其余按首次读取顺序。
        order = [(e["version"], e["path"]) for e in full["evidence"]]
        self.assertEqual(order[0], ("after", "g.py"))
        self.assertEqual(order[1:], sorted(order[1:], key=lambda key: list(reads.first_seen).index(key)))
        payload = json.loads(full["prompt"][len(recheck.PROMPT):])
        self.assertEqual(payload["candidates"][0]["evidence"], [e["id"] for e in full["evidence"]])
        last = full["evidence"][-1]
        answer = json.dumps({"verdicts": [{"id": "C1", "verdict": "excluded", "severity": None,
            "reason": "guard kept", "evidence": [{"source": last["id"], "line": last["end"]}], "missing": ""}]})
        self.assertEqual(recheck.parse_verdicts(answer, full)[0]["verdict"], "excluded")
        # 预算只够前面几段时，后面的段整段丢弃并记录，不截断。
        budget = full["prompt_chars"] - len(full["evidence"][-1]["text"]) - 20
        small = rollout.rollout_packet(self.ledger, reads, self.repo, self.revisions[1], budget)
        self.assertLessEqual(small["prompt_chars"], budget)
        self.assertTrue(small["preparation"]["dropped"])
        kept = {(e["version"], e["path"], e["start"], e["end"]) for e in small["evidence"]}
        dropped = {tuple(d.values()) for d in small["preparation"]["dropped"]}
        self.assertFalse(kept & dropped)
        self.assertEqual(len(kept) + len(dropped), full["preparation"]["spans_total"])

    def pipeline_base(self, meta, protocol=None):
        base = self.root / "pipeline"
        self.write_rollout(base / "initial/home/.kloop/projects/a/b/sessions/x.jsonl", self.fixture_calls())
        (base / "initial").mkdir(parents=True, exist_ok=True)
        (base / "initial/repo").symlink_to(self.repo)
        config = self.root / "config.toml"
        config.write_text('provider="fixture"\n[providers.fixture]\nwire_api="responses"\n'
                          'base_url="http://unused.invalid"\nmodel="fixture"\n'
                          '[providers.fixture.auth_header]\nAuthorization="SECRET_KEY"\n')
        recheck.save_json(base / "meta.json", meta)
        recheck.save_json(base / "ledger.json", self.ledger)
        recheck.save_json(base / "protocol.json", {
            "baseline": self.revisions[0], "target": self.revisions[1], "max_chars": 160_000,
            "provider": "fixture", "sampling": {"model": "fixture", "max_output_tokens": 54321},
            "config_sha256": recheck.digest(config.read_text()), **(protocol or {})})
        (base / "report-a.md").write_text("Initial report\n")
        return base, config

    def args(self, base, config, name):
        return type("Args", (), {"pipeline": str(base), "out": str(self.root / name),
                                 "source": str(self.repo), "config": str(config)})()

    def deliver(self, packet, provider, sampling, folder):
        self.assertEqual(sampling["max_output_tokens"], 54321)
        self.assertEqual(provider["auth_header"]["Authorization"], "SECRET_KEY")
        folder.mkdir()
        source = packet["evidence"][0]
        recheck.save_json(folder / "verdicts.json", [{"id": "C1", "verdict": "confirmed",
            "severity": "low", "reason": "guard dropped", "missing": "",
            "evidence": [{"source": source["id"], "line": source["start"]}]}])
        return {"status": "valid"}

    def test_fixed_window_preparation_failure_still_pairs_after_the_ledger(self):
        # A 组在账本保存后的证据准备阶段失败（如固定窗口超预算），是 A 组未交付，B 组照常跑。
        base, config = self.pipeline_base({"status": "failed", "stage": "evidence", "recheck_requests": 0})
        with mock.patch.object(pipeline, "independent_request", side_effect=self.deliver) as request:
            with contextlib.redirect_stdout(io.StringIO()):
                rollout.command(self.args(base, config, "b1"))
        request.assert_called_once()
        self.assertEqual(recheck.load_json(self.root / "b1/meta.json")["status"], "complete")
        # 没有账本就不是准备阶段失败，不能配对。
        (base / "ledger.json").unlink()
        (base / "rollout-recheck-attempt.json").unlink()
        with self.assertRaisesRegex(ValueError, "not been attempted"):
            rollout.command(self.args(base, config, "b2"))

    def test_recheck_reuses_the_frozen_delivery_mode_of_the_fixed_window_arm(self):
        # A 组用结构化文本交付时，B 组也必须走同一套交付与校验，两组只差证据。
        base, config = self.pipeline_base(
            {"status": "failed", "recheck_requests": 1, "recheck_status": "failed"},
            {"decision_contract": True, "decision_text": True, "repair_decision": True,
             "evidence_tools": False})
        seen = {}

        def investigate(packet, provider, sampling, source, folder, **options):
            seen.update(options)
            folder.mkdir()
            evidence = packet["evidence"][0]
            recheck.save_json(folder / "verdicts.json", [{"id": "C1", "verdict": "excluded", "severity": None,
                "reason": "guard kept", "missing": "", "evidence": [{"source": evidence["id"], "line": evidence["start"]}]}])
            return {"status": "valid", "requests": 2}

        with mock.patch.object(pipeline.review_evidence, "investigate", side_effect=investigate), \
                mock.patch.object(pipeline, "independent_request") as plain:
            with contextlib.redirect_stdout(io.StringIO()):
                rollout.command(self.args(base, config, "b1"))
        plain.assert_not_called()
        self.assertEqual((seen["decision_contract"], seen["decision_text"], seen["repair_decision"],
                          seen["with_tools"]), (True, True, True, False))
        meta = recheck.load_json(self.root / "b1/meta.json")
        self.assertEqual((meta["status"], meta["recheck_requests"]), ("complete", 2))

    def test_recheck_pairs_with_an_attempted_fixed_window_arm_and_never_retries(self):
        base, config = self.pipeline_base({"status": "failed", "recheck_requests": 0})
        with self.assertRaisesRegex(ValueError, "not been attempted"):
            rollout.command(self.args(base, config, "b0"))
        self.assertFalse((self.root / "b0").exists())
        # A 组交付失败也算已尝试：B 组照常配对。
        recheck.save_json(base / "meta.json", {"status": "failed", "recheck_requests": 1,
                                               "recheck_status": "failed"})

        with mock.patch.object(pipeline, "independent_request", side_effect=self.deliver) as request:
            with contextlib.redirect_stdout(io.StringIO()):
                rollout.command(self.args(base, config, "b1"))
            with self.assertRaisesRegex(ValueError, "already attempted"):
                rollout.command(self.args(base, config, "b2"))
        request.assert_called_once()
        meta = recheck.load_json(self.root / "b1/meta.json")
        self.assertEqual((meta["status"], meta["recheck_status"], meta["recheck_requests"]),
                         ("complete", "valid", 1))
        self.assertIn("Independent recheck", (self.root / "b1/report-b.md").read_text())
        self.assertEqual(recheck.load_json(base / "rollout-recheck-attempt.json"),
                         {"out": str(self.root / "b1")})
        self.assertFalse((self.root / "b2").exists())
        self.assertNotIn("SECRET_KEY", "".join(p.read_text() for p in (self.root / "b1").rglob("*")
                                               if p.is_file()))


if __name__ == "__main__":
    unittest.main()
