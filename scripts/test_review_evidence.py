"""有界补查的真实 HTTP 回放、裁决引用与预算回归。"""

import copy
import http.server
import json
import pathlib
import subprocess
import threading
import unittest
from unittest import mock

import review_evidence as evidence
import review_recheck as review
from test_review_sources import SourceFixture


class EvidenceFixture(SourceFixture):
    def sampling(self):
        return {"model": "fixture", "store": False, "stream": True, "max_output_tokens": 54321,
                "reasoning": {"effort": "xhigh", "summary": "auto"}, "include": ["reasoning.encrypted_content"]}

    def final(self, citation="E1"):
        rows = [{"id": ident, "verdict": "excluded", "severity": None, "reason": "Code disproves failure",
                 "evidence": [{"source": citation if ident == "C1" else "E2", "line": 1}], "missing": ""}
                for ident in ("C1", "C2")]
        return {"event": "response.completed", "response": {"status": "completed",
            "usage": {"input_tokens": 10, "output_tokens": 5}, "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": json.dumps({"verdicts": rows})}]}]}}

    def tool_reply(self, ident="call-1", version="after"):
        return {"event": "response.completed", "response": {"status": "completed", "usage": {"output_tokens": 3},
            "output": [{"id": "reasoning-1", "type": "reasoning", "summary": [], "encrypted_content": "OPAQUE_REASONING"},
                {"id": "function-1", "type": "function_call", "status": "completed", "call_id": ident,
                 "name": "source_read", "arguments": json.dumps({"candidate_id": "C1", "version": version,
                    "package": "", "path": "main.rs", "start": 1, "end": 2})}]}}

    def run_case(self, out="case", **kwargs):
        return evidence.investigate(self.packet, {"base_url": "http://unused.invalid", "auth_header": {"Authorization": "SECRET_KEY"}},
                                    self.sampling(), self.repo, self.root / out, cargo_home=self.cargo, **kwargs)


class EvidenceTests(EvidenceFixture):
    def test_control_uses_one_request_and_identical_prompt_without_tools(self):
        bodies = []

        def request(provider, body, timeout, **kwargs):
            bodies.append(copy.deepcopy(body))
            self.assertEqual(provider["auth_header"]["Authorization"], "SECRET_KEY")
            return self.final()

        with mock.patch.object(review, "one_request", side_effect=request) as call:
            control = self.run_case("control")
            treatment = self.run_case("treatment", with_tools=True)
        self.assertEqual(call.call_count, 2)
        self.assertEqual((control["status"], treatment["status"]), ("valid", "valid"))
        self.assertEqual(bodies[0]["input"], bodies[1]["input"])
        self.assertNotIn("tools", bodies[0])
        self.assertIn('"C1": ["E1"]', bodies[0]["input"][0]["content"])
        self.assertIn('"C2": ["E2"]', bodies[0]["input"][0]["content"])
        self.assertEqual(bodies[1]["max_output_tokens"], 54321)
        self.assertEqual(len(bodies[1]["tools"]), 3)
        for name in ("control", "treatment"):
            for path in (self.root / name).rglob("*"):
                if path.is_file():
                    self.assertNotIn("SECRET_KEY", path.read_text())
                    self.assertNotIn("LATER_FIX_SECRET", path.read_text())

    def test_real_child_http_replays_reasoning_and_tool_results(self):
        received, responses = [], [self.tool_reply(), self.final("S1")]

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(handler):
                received.append(json.loads(handler.rfile.read(int(handler.headers["Content-Length"]))))
                result = responses[len(received) - 1]
                payload = ("data: " + json.dumps({"type": result["event"], "response": result["response"]}) + "\n\n").encode()
                handler.send_response(200)
                handler.send_header("Content-Type", "text/event-stream")
                handler.send_header("Content-Length", str(len(payload)))
                handler.end_headers()
                handler.wfile.write(payload)

            def log_message(self, *args):
                pass

        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                meta = evidence.investigate(self.packet, {"base_url": f"http://127.0.0.1:{server.server_port}"},
                    self.sampling(), self.repo, self.root / "http", with_tools=True, cargo_home=self.cargo, max_requests=3)
            finally:
                server.shutdown()
                thread.join()
        self.assertEqual((meta["status"], meta["requests"], meta["tool_calls"]), ("valid", 2, 1))
        self.assertEqual(received[1]["input"][1:3], self.tool_reply()["response"]["output"])
        result = received[1]["input"][3]
        self.assertEqual((result["type"], result["call_id"]), ("function_call_output", "call-1"))
        self.assertEqual(json.loads(result["output"])["sources"][0]["text"], "1: new caller\n2: new guard")
        self.assertEqual(review.load_json(self.root / "http/verdicts.json")[0]["evidence"], [{"source": "S1", "line": 1}])

    def test_last_request_has_no_tools_and_receives_all_pending_results(self):
        bodies, results = [], [self.tool_reply(), self.tool_reply("call-2", "before"), self.final("S2")]

        def request(provider, body, timeout, **kwargs):
            bodies.append(copy.deepcopy(body))
            return results[len(bodies) - 1]

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(with_tools=True, max_requests=3)
        self.assertEqual((meta["status"], meta["requests"]), ("valid", 3))
        self.assertNotIn("tools", bodies[-1])
        ids = [item["call_id"] for item in bodies[-1]["input"] if item.get("type") == "function_call_output"]
        self.assertEqual(ids, ["call-1", "call-2"])
        self.assertIn('"C1": ["E1", "S1", "S2"]', bodies[-1]["input"][-1]["content"])
        self.assertIn('"C2": ["E2"]', bodies[-1]["input"][-1]["content"])

    def test_tool_call_budget_stops_reads_but_preserves_a_final_decision_request(self):
        first = self.tool_reply()
        first["response"]["output"].append(self.tool_reply("call-2")["response"]["output"][-1])
        bodies = []

        def request(provider, body, timeout, **kwargs):
            bodies.append(copy.deepcopy(body))
            return first if len(bodies) == 1 else self.final("S1")

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(with_tools=True, max_calls=1)
        self.assertEqual((meta["status"], meta["tool_calls"], meta["requests"]), ("valid", 1, 2))
        self.assertNotIn("tools", bodies[-1])
        outputs = [json.loads(item["output"]) for item in bodies[-1]["input"] if item.get("type") == "function_call_output"]
        self.assertEqual([v["status"] for v in outputs], ["ok", "unavailable"])

    def test_missing_source_is_returned_as_a_gap_not_a_fake_empty_file(self):
        first = self.tool_reply()
        call = first["response"]["output"][-1]
        args = json.loads(call["arguments"])
        args.update(package="widget@99.0.0", path="src/lib.rs")
        call["arguments"] = json.dumps(args)
        bodies = []

        def request(provider, body, timeout, **kwargs):
            bodies.append(copy.deepcopy(body))
            return first if len(bodies) == 1 else self.final()

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(with_tools=True)
        self.assertEqual(meta["status"], "valid")
        result = json.loads(bodies[1]["input"][3]["output"])
        self.assertEqual(result["status"], "unavailable")
        self.assertIn("missing or ambiguous", result["missing"])
        self.assertNotIn("sources", result)

    def test_hallucinated_source_citation_is_rejected_without_retry(self):
        with mock.patch.object(review, "one_request", return_value=self.final("S99")) as request:
            meta = self.run_case(with_tools=True)
        request.assert_called_once()
        self.assertEqual(meta["status"], "failed")
        self.assertTrue((self.root / "case/evidence-packet.json").exists())

    def test_transport_and_invalid_output_fail_without_retry(self):
        for name, value in (("transport", {"event": "request.error", "error_type": "URLError"}),
                            ("incomplete", {"event": "response.incomplete", "response": {"status": "incomplete"}})):
            with self.subTest(name=name), mock.patch.object(review, "one_request", return_value=value) as request:
                meta = self.run_case(name, with_tools=True)
            self.assertEqual((meta["status"], meta["requests"]), ("failed", 1))
            request.assert_called_once()

    def test_empty_candidates_skip_sources_and_network(self):
        packet = review.make_packet({"name": "empty", "split": "unscored", "target": self.revisions[1], "candidates": []}, self.repo)
        with mock.patch.object(review, "one_request") as request:
            meta = evidence.investigate(packet, {}, {}, self.root / "nonexistent", self.root / "empty", with_tools=True)
        request.assert_not_called()
        self.assertEqual((meta["status"], meta["requests"]), ("skipped-empty", 0))

    def test_forged_frozen_source_fails_before_network_even_with_resealed_packet(self):
        self.packet["evidence"][0]["text"] = "fabricated source"
        self.packet.pop("packet_sha256")
        self.packet["packet_sha256"] = review.digest(json.dumps(self.packet, ensure_ascii=False, sort_keys=True))
        with mock.patch.object(review, "one_request") as request:
            meta = self.run_case(with_tools=True)
        request.assert_not_called()
        self.assertEqual(meta["status"], "failed")


if __name__ == "__main__":
    unittest.main()
