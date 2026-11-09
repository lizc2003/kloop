"""候选复核实验的隔离、证据边界与请求预算回归。"""

import argparse
import contextlib
import io
import http.server
import json
import pathlib
import subprocess
import tempfile
import threading
import unittest
from unittest import mock

import review_recheck as recheck


class RecheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.revisions = []
        for content in ("old caller\nold guard\n", "new caller\nmissing guard\n", "later fix\nnew guard\n"):
            (self.repo / "code.txt").write_text(content)
            self.git("add", "code.txt")
            self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                     "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
                     "commit", "-qm", "Fixture")
            self.revisions.append(self.git("rev-parse", "HEAD").strip())
        self.manifest = {
            "name": "private-group-label", "split": "development", "target": self.revisions[1],
            "candidates": [{
                "id": "C1", "question": "Does the new caller preserve the guard?", "expected": "bug",
                "original_reason": "SECRET_ORIGINAL_VERDICT", "provenance": "SECRET_SOURCE_REPORT",
                "evidence": [{"version": version, "path": "code.txt", "start": 1, "end": 2}
                             for version in ("before", "after")],
            }],
        }

    def git(self, *args):
        return recheck.git(self.repo, *args)

    def packet(self):
        return recheck.make_packet(self.manifest, self.repo)

    def verdict(self, **updates):
        row = {"id": "C1", "verdict": "confirmed", "severity": "medium", "reason": "Guard removed",
               "evidence": [{"source": "E2", "line": 2}], "missing": ""}
        return {**row, **updates}

    def parse(self, rows):
        return recheck.parse_verdicts(json.dumps({"verdicts": rows}), self.packet())

    def test_freezes_target_and_parent_and_blinds_labels(self):
        packet = self.packet()
        prompt = packet["prompt"]
        for forbidden in ("later fix", "new guard", "SECRET_", "private-group-label", '"expected"',
                          '"split"', '"original_reason"', self.revisions[2]):
            self.assertNotIn(forbidden, prompt)
        self.assertIn("1: old caller", prompt)
        self.assertIn("2: missing guard", prompt)
        self.assertEqual(packet["baseline"], self.revisions[0])
        self.assertEqual(packet["target"], self.revisions[1])

    def test_oversize_and_invalid_evidence_fail_without_truncation(self):
        with self.assertRaisesRegex(ValueError, "budget"):
            recheck.make_packet(self.manifest, self.repo, max_chars=10)
        self.manifest["candidates"][0]["evidence"][0]["end"] = 3
        with self.assertRaisesRegex(ValueError, "exceeds"):
            self.packet()

    def test_duplicate_candidates_are_rejected(self):
        self.manifest["candidates"] *= 2
        with self.assertRaisesRegex(ValueError, "unique"):
            self.packet()

    def test_requires_exact_candidate_set_and_real_source_lines(self):
        for rows in ([], [self.verdict(), self.verdict()], [self.verdict(id="other")],
                     [self.verdict(evidence=[])],
                     [self.verdict(evidence=[{"source": "E2", "line": 3}])],
                     [self.verdict(evidence=[{"source": "E99", "line": 1}])]):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                self.parse(rows)

    def test_uncertainty_is_separate_from_severity(self):
        row = self.verdict(verdict="unresolved", severity=None, evidence=[], missing="Caller absent")
        self.assertEqual(self.parse([row]), [row])
        for update in ({"severity": "low"}, {"missing": ""}):
            with self.subTest(update=update), self.assertRaises(ValueError):
                self.parse([{**row, **update}])
        with self.assertRaises(ValueError):
            self.parse([self.verdict(verdict="excluded", severity=None, evidence=[])])

    def args(self):
        packet_dir = self.root / "packet"
        packet_dir.mkdir()
        recheck.save_json(packet_dir / "packet.json", self.packet())
        config = self.root / "config.toml"
        config.write_text('provider = "fixture"\n[providers.fixture]\nwire_api = "responses"\n'
                          'base_url = "http://unused.invalid"\nmodel = "fixture-model"\n'
                          '[providers.fixture.auth_header]\nAuthorization = "SECRET_KEY"\n')
        return argparse.Namespace(packet=str(packet_dir), out=str(self.root / "out"),
                                  config=str(config), provider=None, model=None, effort="high",
                                  trials=1, timeout=120, max_output_tokens=8192)

    def completed(self, answer):
        return {"event": "response.completed", "response": {
            "status": "completed", "usage": {"input_tokens": 100, "output_tokens": 20},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": answer}]}],
        }}

    def run_recheck(self, args):
        with contextlib.redirect_stdout(io.StringIO()):
            recheck.recheck(args)

    def test_one_request_and_no_credentials_or_truth_in_artifacts(self):
        args = self.args()
        answer = json.dumps({"verdicts": [self.verdict()]})
        with mock.patch.object(recheck, "one_request", return_value=self.completed(answer)) as request:
            self.run_recheck(args)
        request.assert_called_once()
        provider, body, timeout = request.call_args.args
        self.assertEqual(provider["auth_header"]["Authorization"], "SECRET_KEY")
        self.assertEqual(timeout, 120)
        self.assertNotIn('"expected"', json.dumps(body))
        for path in pathlib.Path(args.out).rglob("*"):
            if path.is_file():
                self.assertNotIn("SECRET_KEY", path.read_text())
        score = recheck.score_run(args.out)
        self.assertEqual(score["trials"][0]["confirmed_bugs"], 1)
        self.assertEqual(score["trials"][0]["status"], "valid")

    def test_child_worker_reads_local_sse_and_sends_only_blinded_packet(self):
        args = self.args()
        args.max_output_tokens = 16384
        response = self.completed(json.dumps({"verdicts": [self.verdict()]}))
        received = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                payload = ("data: " + json.dumps({"type": response["event"],
                                                "response": response["response"]}) + "\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *unused):
                pass

        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                config = pathlib.Path(args.config)
                config.write_text(config.read_text().replace("http://unused.invalid",
                                  f"http://127.0.0.1:{server.server_port}"))
                self.run_recheck(args)
            finally:
                server.shutdown()
                thread.join()
        self.assertEqual(len(received), 1)
        self.assertEqual(received[0]["max_output_tokens"], 16384)
        self.assertNotIn("SECRET_ORIGINAL_VERDICT", json.dumps(received))
        self.assertNotIn('"expected"', json.dumps(received))
        self.assertEqual(recheck.score_run(args.out)["trials"][0]["status"], "valid")

    def test_output_budget_changes_only_the_request_limit(self):
        args = self.args()
        response = self.completed(json.dumps({"verdicts": [self.verdict()]}))
        with mock.patch.object(recheck, "one_request", return_value=response) as request:
            self.run_recheck(args)
            args.out = str(self.root / "larger-output-budget")
            args.max_output_tokens = 16384
            self.run_recheck(args)
        self.assertEqual(request.call_count, 2)
        first, second = [call.args[1] for call in request.call_args_list]
        self.assertEqual(first["max_output_tokens"], 8192)
        self.assertEqual(second, {**first, "max_output_tokens": 16384})
        self.assertEqual([call.args[2] for call in request.call_args_list], [120, 120])
        meta = recheck.load_json(pathlib.Path(args.out) / "trial-1/meta.json")
        self.assertEqual(meta["max_output_tokens"], 16384)

    def test_nonpositive_output_budget_stops_before_request(self):
        args = self.args()
        args.max_output_tokens = 0
        with mock.patch.object(recheck, "one_request") as request:
            with self.assertRaisesRegex(ValueError, "positive"):
                self.run_recheck(args)
        request.assert_not_called()

    def test_invalid_and_incomplete_responses_are_failed_without_retry(self):
        args = self.args()
        for index, result in enumerate((self.completed("not json"),
                                        {"event": "response.incomplete", "response": {"status": "incomplete"}},
                                        {"event": "response.completed", "response": ["bad envelope"]})):
            args.out = str(self.root / f"failure-{index}")
            with mock.patch.object(recheck, "one_request", return_value=result) as request:
                self.run_recheck(args)
            request.assert_called_once()
            self.assertEqual(recheck.score_run(args.out)["trials"][0]["status"], "failed")

    def test_timeout_is_failed_without_retry(self):
        args = self.args()
        with mock.patch.object(recheck, "one_request", side_effect=subprocess.TimeoutExpired("worker", 120)) as request:
            self.run_recheck(args)
        request.assert_called_once()
        meta = recheck.load_json(pathlib.Path(args.out) / "trial-1/meta.json")
        self.assertEqual(meta["status"], "failed")
        self.assertEqual(meta["error_type"], "TimeoutExpired")

    def test_empty_group_does_not_read_credentials_or_request(self):
        self.manifest["candidates"] = []
        args = self.args()
        args.config = str(self.root / "does-not-exist")
        with mock.patch.object(recheck, "one_request") as request:
            self.run_recheck(args)
        request.assert_not_called()
        self.assertEqual(recheck.load_json(pathlib.Path(args.out) / "skipped.json")["requests"], 0)

    def test_frozen_prompt_tampering_stops_before_request(self):
        args = self.args()
        path = pathlib.Path(args.packet) / "packet.json"
        packet = recheck.load_json(path)
        packet["prompt"] += "tampered"
        recheck.save_json(path, packet)
        with mock.patch.object(recheck, "one_request") as request:
            with self.assertRaisesRegex(ValueError, "checksum"):
                self.run_recheck(args)
        request.assert_not_called()

    def test_truth_or_evidence_tampering_stops_before_request(self):
        args = self.args()
        path = pathlib.Path(args.packet) / "packet.json"
        packet = recheck.load_json(path)
        packet["candidates"][0]["expected"] = "not_bug"
        recheck.save_json(path, packet)
        with mock.patch.object(recheck, "one_request") as request:
            with self.assertRaisesRegex(ValueError, "packet checksum"):
                self.run_recheck(args)
        request.assert_not_called()


if __name__ == "__main__":
    unittest.main()
