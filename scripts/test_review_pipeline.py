"""自动复核的版本边界、盲评、空候选与端到端 HTTP 回归。"""

import argparse
import contextlib
import copy
import http.server
import importlib.util
import io
import json
import pathlib
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

import review_pipeline as pipeline
import review_recheck as recheck


spec = importlib.util.spec_from_file_location("review_bench", pathlib.Path(__file__).with_name("review-bench.py"))
bench = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = bench
spec.loader.exec_module(bench)


class PipelineTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-qb", "main")
        self.revisions = []
        for content in ("caller\nguard\nreturn old\n", "inserted\ncaller\nreturn new\n", "LATER_FIX_SECRET\n"):
            (self.repo / "code.txt").write_text(content)
            if not self.revisions:
                (self.repo / "deleted.txt").write_text("old only\n")
                (self.repo / "nested").mkdir()
                (self.repo / "nested/leaf.txt").write_text("leaf\n")
            elif len(self.revisions) == 1:
                (self.repo / "deleted.txt").unlink()
                (self.repo / "added.txt").write_text("new only\n")
            self.git("add", "-A")
            self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                     "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
                     "commit", "-qm", "Fixture")
            self.revisions.append(self.git("rev-parse", "HEAD").strip())
        self.ledger = {"version": 1, "candidates": [{
            "id": "C1", "question": "Can the caller run without the guard?", "verdict": "excluded",
            "reason": "PRIVATE_EXCLUSION_REASON", "evidence": [
                {"version": "after", "path": "code.txt", "line": 3}]}]}

    def git(self, *args):
        return recheck.git(self.repo, *args)

    def report(self, ledger=None):
        return "Initial report\n```review-ledger\n" + json.dumps(ledger or self.ledger) + "\n```\n"

    def packet(self, ledger=None, **kwargs):
        return pipeline.automatic_packet(ledger or self.ledger, self.repo, self.revisions[1], **kwargs)

    def test_automatic_sources_are_frozen_and_blind(self):
        ledger = pipeline.parse_ledger(self.report())
        packet = self.packet(ledger, context=0)
        self.assertEqual(packet["baseline"], self.revisions[0])
        self.assertEqual(packet["target"], self.revisions[1])
        self.assertEqual([(e["version"], e["path"], e["start"], e["end"])
                          for e in packet["evidence"]],
                         [("after", "code.txt", 3, 3), ("before", "code.txt", 1, 3)])
        self.assertIn("guard", packet["prompt"])
        for secret in ("PRIVATE_EXCLUSION_REASON", "LATER_FIX_SECRET", self.revisions[2],
                       '"original_reason"', '"expected"'):
            self.assertNotIn(secret, packet["prompt"])
        payload = json.loads(packet["prompt"][len(recheck.PROMPT):])
        self.assertNotIn("verdict", payload["candidates"][0])
        self.assertEqual(packet["candidates"][0]["original_reason"], "PRIVATE_EXCLUSION_REASON")

    def test_equal_line_mapping_and_window_merging(self):
        row = self.ledger["candidates"][0]
        row["evidence"] = [{"version": "after", "path": "code.txt", "line": line} for line in (1, 2, 3)]
        packet = self.packet(context=1)
        self.assertEqual([(e["version"], e["start"], e["end"]) for e in packet["evidence"]],
                         [("after", 1, 3), ("before", 1, 3)])
        row["evidence"] = [{"version": "after", "path": "code.txt", "line": 2}]
        packet = self.packet(context=0)
        self.assertEqual([(e["version"], e["start"], e["end"]) for e in packet["evidence"]],
                         [("after", 2, 2), ("before", 1, 1)])

    def test_overlapping_windows_across_candidates_share_the_same_source(self):
        self.ledger["candidates"][0]["evidence"][0]["line"] = 1
        other = copy.deepcopy(self.ledger["candidates"][0])
        other["id"] = "C2"
        other["evidence"][0]["line"] = 3
        self.ledger["candidates"].append(other)
        packet = self.packet(context=1)
        self.assertEqual([(e["version"], e["start"], e["end"]) for e in packet["evidence"]],
                         [("after", 1, 3), ("before", 1, 3)])
        payload = json.loads(packet["prompt"][len(recheck.PROMPT):])
        self.assertEqual([row["evidence"] for row in payload["candidates"]], [["E1", "E2"], ["E1", "E2"]])

    def test_added_and_deleted_sources_record_absence(self):
        self.ledger["candidates"][0]["evidence"] = [
            {"version": "before", "path": "deleted.txt", "line": 1},
            {"version": "after", "path": "added.txt", "line": 1}]
        packet = self.packet()
        payload = json.loads(packet["prompt"][len(recheck.PROMPT):])
        self.assertEqual(payload["source_gaps"], [
            {"candidate": "C1", "version": "after", "path": "deleted.txt", "state": "absent"},
            {"candidate": "C1", "version": "before", "path": "added.txt", "state": "absent"}])

    def test_missing_or_invalid_ledger_is_not_an_empty_list(self):
        for report in ("no ledger", self.report() + self.report(),
                       '```review-ledger\n{"version":1}\n```'):
            with self.subTest(report=report), self.assertRaises(ValueError):
                pipeline.parse_ledger(report)
        for mutation in (lambda row: row.update(evidence=[]),
                         lambda row: row["evidence"][0].update(path="../secret"),
                         lambda row: row["evidence"][0].update(line=True),
                         lambda row: row.update(verdict="downgraded")):
            ledger = copy.deepcopy(self.ledger)
            mutation(ledger["candidates"][0])
            with self.assertRaises(ValueError):
                pipeline.parse_ledger(self.report(ledger))

    def test_invalid_anchor_and_oversize_fail_without_truncation(self):
        with self.assertRaisesRegex(ValueError, "budget"):
            self.packet(max_chars=100)
        self.ledger["candidates"][0]["evidence"][0]["line"] = 4
        with self.assertRaisesRegex(ValueError, "invalid frozen"):
            self.packet()

    def test_directory_listing_is_not_accepted_as_source_code(self):
        self.ledger["candidates"][0]["evidence"] = [{"version": "after", "path": "nested", "line": 1}]
        with self.assertRaisesRegex(ValueError, "Git blob"):
            self.packet()

    def test_confirmed_and_unresolved_candidates_are_not_rechecked(self):
        for verdict in ("confirmed", "unresolved"):
            self.ledger["candidates"][0]["verdict"] = verdict
            self.assertEqual(self.packet()["candidates"], [])

    def test_endpoint_replacement_preserves_every_other_setting(self):
        text = ('provider="quoted.name"\n[providers.other]\nbase_url="http://keep.invalid"\n'
                '[providers."quoted.name"]\nbase_url="http://replace.invalid"\nmodel="test"\n'
                '[providers."quoted.name".auth_header]\nAuthorization="SECRET"\n')
        changed = pipeline.replace_endpoint(text, "quoted.name", "http://127.0.0.1:42")
        self.assertIn('base_url="http://keep.invalid"', changed)
        self.assertIn('Authorization="SECRET"', changed)
        self.assertNotIn("replace.invalid", changed)

    def run_http_pipeline(self, ledger=None, malformed=False, preparation_failure=False, evidence_tools=False,
                          decision_contract=False):
        received = []
        initial_report = "no structured ledger" if malformed else self.report(ledger)

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(handler):
                body = json.loads(handler.rfile.read(int(handler.headers["Content-Length"])))
                received.append(body)
                if len(received) == 1:
                    answer = initial_report
                else:
                    evidence = json.loads(body["input"][0]["content"].split(recheck.PROMPT, 1)[1])["evidence"][0]
                    answer = json.dumps({"verdicts": [{"id": "C1", "verdict": "confirmed",
                        "severity": "medium", "reason": "caller no longer checks the guard",
                        "evidence": [{"source": evidence["id"], "line": evidence["start"]}], "missing": ""}]})
                response = {"type": "response.completed", "response": {"status": "completed",
                    "output": [{"type": "message", "content": [{"type": "output_text", "text": answer}]}],
                    "usage": {"input_tokens": 31, "output_tokens": 7}}}
                if decision_contract and len(received) > 1:
                    row = json.loads(answer)["verdicts"][0]
                    ident = row.pop("id")
                    row.update(basis="introduced_or_worsened", comparison={"input": "unguarded call",
                        "before": "guard stops the call", "after": "call runs without the guard"})
                    response["response"]["output"] = [{"type": "function_call", "name": "submit_review",
                        "call_id": "submit-1", "arguments": json.dumps({ident: row})}]
                payload = ("data: " + json.dumps(response) + "\n\n").encode()
                handler.send_response(200)
                handler.send_header("Content-Type", "text/event-stream")
                handler.send_header("Content-Length", str(len(payload)))
                handler.end_headers()
                handler.wfile.write(payload)

            def log_message(self, *args):
                pass

        fake = self.root / "fake-kloop"
        fake.write_text(f"#!{sys.executable}\n" + '''
import json, os, pathlib, sys, tomllib, urllib.request
state = pathlib.Path(os.environ['HOME']) / '.kloop'
config = tomllib.loads((state / 'config.toml').read_text())
provider = config['providers'][config['provider']]
body = {'model':provider['model'], 'stream':True, 'store':False,
        'max_output_tokens':provider['max_output_tokens'],
        'reasoning':{'effort':provider['effort'], 'summary':'auto'},
        'instructions':'PRIVATE_INITIAL_INSTRUCTIONS', 'tools':[],
        'input':[{'role':'user','content':sys.argv[-1]}], 'prompt_cache_key':'initial-only'}
request = urllib.request.Request(provider['base_url'] + '/responses',
    data=json.dumps(body).encode(),headers={'Content-Type':'application/json'})
with urllib.request.urlopen(request) as response:
    events = [json.loads(line[5:].strip()) for line in response if line.startswith(b'data:')]
response = events[-1]['response']
folder = state / 'projects/a/b/sessions'
folder.mkdir(parents=True)
rows = [{'type':'provider_usage','usage':response['usage']},
        {'type':'message','role':'assistant','content':[
            {'type':'text','text':part['text']} for part in response['output'][0]['content']]}]
(folder / 'fixture.jsonl').write_text(''.join(json.dumps(row)+'\\n' for row in rows))
''')
        fake.chmod(0o700)
        config = self.root / "config.toml"
        out = self.root / "out"
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            config.write_text(f'provider="fixture"\n[providers.fixture]\nwire_api="responses"\n'
                              f'base_url="http://127.0.0.1:{server.server_port}"\nmodel="fixture"\n'
                              'effort="xhigh"\nmax_output_tokens=54321\n'
                              '[providers.fixture.auth_header]\nAuthorization="SECRET_KEY"\n')
            args = argparse.Namespace(out=str(out), commit=self.revisions[1], source=str(self.repo),
                skill=str(pathlib.Path(__file__).parents[1] / "rust/crates/core/src/skills/code-review/SKILL.md"),
                config=str(config), kloop=str(fake), max_rounds=12, timeout=10, context=1, max_chars=20_000)
            args.evidence_tools = evidence_tools
            args.decision_contract = decision_contract
            try:
                with contextlib.redirect_stdout(io.StringIO()):
                    try:
                        if preparation_failure:
                            with mock.patch.object(pipeline, "automatic_packet", side_effect=ValueError("budget")):
                                pipeline.run(args, bench)
                        else:
                            pipeline.run(args, bench)
                    except SystemExit as error:
                        self.assertEqual(error.code, 1)
            finally:
                server.shutdown()
                thread.join()
        return out, received

    def test_full_pipeline_with_native_probe_and_child_http_request(self):
        out, received = self.run_http_pipeline()
        self.assertEqual(len(received), 2)
        self.assertEqual(recheck.load_json(out / "meta.json")["status"], "complete")
        self.assertEqual(recheck.load_json(out / "protocol.json")["sampling"], {
            "model": "fixture", "stream": True, "store": False, "max_output_tokens": 54321,
            "reasoning": {"effort": "xhigh", "summary": "auto"}})
        self.assertEqual(received[1]["max_output_tokens"], 54321)
        for value in ("PRIVATE_INITIAL_INSTRUCTIONS", "PRIVATE_EXCLUSION_REASON", "SECRET_KEY",
                      "LATER_FIX_SECRET", "initial-only"):
            self.assertNotIn(value, json.dumps(received[1]))
        self.assertEqual(received[1]["input"][0]["content"], (out / "recheck-prompt.txt").read_text())
        self.assertTrue((out / "report-b.md").read_text().startswith((out / "report-a.md").read_text()))
        self.assertFalse(list(out.rglob("config.toml")))
        for path in out.rglob("*"):
            if path.is_file() and ".git" not in path.parts:
                self.assertNotIn(b"SECRET_KEY", path.read_bytes())

    def test_empty_ledger_skips_remote_recheck(self):
        out, received = self.run_http_pipeline({"version": 1, "candidates": []})
        self.assertEqual(len(received), 1)
        self.assertEqual(recheck.load_json(out / "meta.json")["recheck_status"], "skipped-empty")
        self.assertEqual((out / "report-a.md").read_text(), (out / "report-b.md").read_text())

    def test_pipeline_can_explicitly_enable_frozen_evidence_followup(self):
        out, received = self.run_http_pipeline(evidence_tools=True)
        self.assertEqual(len(received), 2)
        self.assertEqual(recheck.load_json(out / "meta.json")["status"], "complete")
        self.assertEqual(len(received[1]["tools"]), 3)
        self.assertTrue(recheck.load_json(out / "protocol.json")["evidence_tools"])

    def test_pipeline_can_freeze_and_render_the_decision_contract(self):
        out, received = self.run_http_pipeline(evidence_tools=True, decision_contract=True)
        self.assertEqual(recheck.load_json(out / "meta.json")["status"], "complete")
        self.assertTrue(recheck.load_json(out / "protocol.json")["decision_contract"])
        self.assertEqual(received[1]["tools"][-1]["name"], "submit_review")
        self.assertIn("Before: guard stops the call", (out / "report-b.md").read_text())

    def test_missing_ledger_records_failure_without_recheck(self):
        out, received = self.run_http_pipeline(malformed=True)
        self.assertEqual(len(received), 1)
        self.assertEqual(recheck.load_json(out / "meta.json")["stage"], "candidate-ledger")
        self.assertFalse((out / "report-b.md").exists())

    def test_resume_preserves_initial_failure_and_reuses_frozen_ledger_and_budget(self):
        base, received = self.run_http_pipeline(preparation_failure=True)
        self.assertEqual(len(received), 1)
        original = (base / "meta.json").read_text()
        self.assertEqual(json.loads(original)["error_detail"], "budget")
        out = self.root / "resumed"
        args = argparse.Namespace(initial=str(base), out=str(out), source=str(self.repo),
                                  config=str(self.root / "config.toml"))

        def deliver(packet, provider, sampling, folder):
            self.assertEqual(sampling["max_output_tokens"], 54321)
            folder.mkdir()
            row = {"id": "C1", "verdict": "unresolved", "severity": None, "reason": "caller missing",
                   "evidence": [], "missing": "caller invocation"}
            recheck.save_json(folder / "verdicts.json", [row])
            return {"status": "valid"}

        with mock.patch.object(pipeline, "independent_request", side_effect=deliver) as request:
            with contextlib.redirect_stdout(io.StringIO()):
                pipeline.resume(args)
        request.assert_called_once()
        self.assertEqual((base / "meta.json").read_text(), original)
        self.assertEqual((out / "report-a.md").read_text(), (base / "report-a.md").read_text())
        meta = recheck.load_json(out / "meta.json")
        self.assertEqual(meta["status"], "complete")
        self.assertGreaterEqual(meta["wall_s"], recheck.load_json(base / "meta.json")["wall_s"])

    def test_resume_refuses_a_prior_request_or_changed_ledger(self):
        base, _ = self.run_http_pipeline(preparation_failure=True)
        args = argparse.Namespace(initial=str(base), out=str(self.root / "resumed"), source=str(self.repo),
                                  config=str(self.root / "config.toml"))
        meta = recheck.load_json(base / "meta.json")
        meta["recheck_requests"] = 1
        recheck.save_json(base / "meta.json", meta)
        with self.assertRaisesRegex(ValueError, "already attempted"):
            pipeline.resume(args)
        meta["recheck_requests"] = 0
        recheck.save_json(base / "meta.json", meta)
        (base / "ledger.json").write_text('{"version":1,"candidates":[]}')
        with self.assertRaisesRegex(ValueError, "changed"):
            pipeline.resume(args)

    def test_worker_enforces_body_deadline_even_when_idle_budget_is_larger(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(handler):
                handler.rfile.read(int(handler.headers["Content-Length"]))
                handler.send_response(200)
                handler.send_header("Content-Type", "text/event-stream")
                handler.end_headers()
                threading.Event().wait(0.3)

            def log_message(self, *args):
                pass

        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                result = recheck.one_request({"base_url": f"http://127.0.0.1:{server.server_port}"},
                    {"model": "fixture"}, 5, open_timeout=2, idle_timeout=2, body_wall_timeout=0.05)
            finally:
                server.shutdown()
                thread.join()
        self.assertEqual(result["event"], "request.error")
        self.assertEqual(result["error_type"], "TimeoutError")


if __name__ == "__main__":
    unittest.main()
