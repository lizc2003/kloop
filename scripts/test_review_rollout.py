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
            ("bash", {"command": "grep -rn x ."}, "nope.py:3:x\n"),
            ("bash", {"command": "sed -n '50,60p' f.py"}, ""),
            ("bash", {"command": "git show HEAD:missing.py"}, "fatal"),
            ("bash", {"command": "python3 -c 'print(1)'"}, "1\n"),
        ]

    def test_only_reads_located_in_frozen_blobs_are_collected(self):
        path = self.root / "rollout.jsonl"
        self.write_rollout(path, self.fixture_calls())
        reads = rollout.read_regions(path, self.repo, self.repo, self.versions)
        self.assertEqual(reads.lines, {
            ("after", "f.py"): {2, 3, 6, 7, 5, 9, 12},
            ("before", "f.py"): {10, 11, 5},
            ("after", "g.py"): {3, 4},
            ("after", "h.py"): {1, 2, 3},
        })
        self.assertEqual(list(reads.first_seen), [("after", "f.py"), ("before", "f.py"),
                                                  ("after", "g.py"), ("after", "h.py")])

    def test_truncated_patch_only_counts_shown_lines(self):
        diff = self.git("diff", "-U0", "HEAD^", "HEAD", "--", "f.py")
        cut = diff[:diff.index("+line 5 changed")]
        reads = rollout.Reads(self.repo, self.repo, self.versions)
        reads.patch(cut)
        self.assertEqual(reads.lines, {("before", "f.py"): {5}})

    def test_only_lines_actually_shown_in_the_output_are_counted(self):
        notice = "\n[output truncated: 9000 characters and 0 additional bytes omitted]"
        diff = self.git("diff", "-U0", "HEAD^", "HEAD", "--", "f.py")
        pointer = ("[output truncated: 40000 chars, 900 lines; full output saved to /tmp/off-1.txt]\n"
                   + diff + "\n[Query it in place instead of reading it back: bash with `python3 -c '...'` "
                   "over that path, printing only the fields you need, so only what you extract enters the context]")
        calls = [
            # 截断：只计显示出的前三行。
            ("bash", {"command": "sed -n '1,12p' f.py"}, "line 1\nline 2\nline 3" + notice),
            # 管道过滤：`| head -n 1` 只显示一行。
            ("bash", {"command": "sed -n '4,12p' f.py | head -n 1"}, "line 4\n"),
            # 反向范围：sed 只打印起始行。
            ("bash", {"command": "sed -n '5,3p' f.py"}, "line 5 changed\n"),
            # 条件分支没执行：没有输出，一行不计。
            ("bash", {"command": "false && sed -n '6,7p' f.py"}, ""),
            # 多个读取段分不清输出归属：g.py 与 h.py 开头相同、g.py 那段没执行，整条不计，
            # 也不能把 h.py 的输出记到 g.py 上。
            ("bash", {"command": "false && sed -n '1,5p' g.py; cat h.py"}, "line 1\nline 2\nline 3\n"),
            ("bash", {"command": "sed -n '1,2p' g.py && echo --- && sed -n '3,3p' g.py"},
             "line 1\nline 2\n---\nline 3\n"),
            # 只有一个已识别读取，但还有别的段或阶段能产生输出：归属不明，整条不计。
            ("bash", {"command": "false && sed -n '1,5p' g.py; head -n 3 h.py"}, "line 1\nline 2\nline 3\n"),
            ("bash", {"command": "sed -n '1,5p' g.py | paste - h.py"}, "line 1\tline 1\n"),
            # nl 的行号前缀去掉后比较，nl 在管道中间也一样。
            ("bash", {"command": "nl -ba g.py | sed -n '4,5p'"}, "     4\tline 4\n     5\tline 5\n"),
            ("bash", {"command": "git show HEAD^:f.py | nl -ba | sed -n '2,3p'"},
             "     2\tline 2\n     3\tline 3\n"),
            # 整个文件被截断：只计前两行。
            ("bash", {"command": "cat h.py"}, "line 1\nline 2" + notice),
            # 会话落盘预览：连补丁也不计。
            ("bash", {"command": "git diff -U0 HEAD^ HEAD"}, pointer),
            # 解析器假设之外的输入：这一条不计，不中断整次准备。
            ("read_file", {"path": None}, "1\tline 1"),
        ]
        path = self.root / "shown.jsonl"
        self.write_rollout(path, calls)
        reads = rollout.read_regions(path, self.repo, self.repo, self.versions)
        self.assertEqual(reads.lines, {
            ("after", "f.py"): {1, 2, 3, 4, 5},
            ("before", "f.py"): {2, 3},
            ("after", "g.py"): {4, 5},
            ("after", "h.py"): {1, 2},
        })
        self.assertEqual(reads.counts["unattributed-skipped"], 7)
        self.assertEqual(reads.counts["offloaded-skipped"], 1)
        self.assertEqual(reads.counts["unparsed"], 1)

    def test_longest_shown_prefix_starts_at_the_first_expected_line(self):
        self.assertEqual(rollout.longest_shown_prefix(["a", "b", "c"], ["x", "a", "b", "y", "a", "b", "c"]), 3)
        self.assertEqual(rollout.longest_shown_prefix(["a", "b"], ["b", "a"]), 1)
        self.assertEqual(rollout.longest_shown_prefix(["a"], ["b"]), 0)
        self.assertEqual(rollout.longest_shown_prefix([], ["a"]), 0)

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
