"""结构化提交的授权、行为比较、预算终止及真实 HTTP 回归。"""

import copy
import http.server
import json
import threading
import unittest
from unittest import mock

import review_contract as contract
import review_evidence as evidence
import review_recheck as review
from test_review_evidence import EvidenceFixture


class ContractTests(EvidenceFixture):
    def values(self, citation="E1"):
        rows = json.loads(self.final(citation)["response"]["output"][0]["content"][0]["text"])["verdicts"]
        return {row.pop("id"): {**row, "comparison": {"input": "caller with guard",
                   "before": "guard prevents the claimed failure", "after": "guard still prevents it"},
                   "basis": "disproved"} for row in rows}

    def submit(self, citation="E1"):
        value = self.final()
        value["response"]["output"] = [{"type": "function_call", "name": contract.NAME,
            "call_id": "submit-1", "arguments": json.dumps(self.values(citation))}]
        return value

    def parse(self, value):
        return contract.parse_submission(json.dumps(value), self.packet)

    def text_reply(self, value=None):
        response = self.final()
        response["response"]["output"][0]["content"][0]["text"] = json.dumps(self.values() if value is None else value)
        return response

    def test_text_decision_uses_the_same_schema_sampling_and_local_validator(self):
        bodies = []

        def request(provider, body, *args, **kwargs):
            bodies.append(copy.deepcopy(body))
            return self.text_reply()

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(decision_contract=True, decision_text=True)
        self.assertEqual((meta["status"], meta["requests"], meta["tool_calls"]), ("valid", 1, 0))
        self.assertEqual(bodies[0]["text"]["format"], {"type": "json_schema", "name": "review_decisions",
            "strict": True, "schema": contract.submission_tool(self.packet)["parameters"]})
        self.assertNotIn("tools", bodies[0])
        self.assertEqual({k: bodies[0][k] for k in self.sampling()}, self.sampling())
        self.assertEqual(review.load_json(self.root / "case/verdicts.json"), self.parse(self.values()))

    def test_text_decision_queries_sources_and_updates_final_schema(self):
        bodies = []

        def request(provider, body, *args, **kwargs):
            bodies.append(copy.deepcopy(body))
            return self.tool_reply() if len(bodies) == 1 else self.text_reply(self.values("S1"))

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(decision_contract=True, decision_text=True, with_tools=True, max_requests=2)
        self.assertEqual((meta["status"], meta["requests"], meta["tool_calls"]), ("valid", 2, 1))
        self.assertEqual([t["name"] for t in bodies[0]["tools"]],
                         ["source_packages", "source_search", "source_read"])
        self.assertEqual(bodies[0]["tool_choice"], "auto")
        self.assertNotIn("tools", bodies[-1])
        citations = bodies[-1]["text"]["format"]["schema"]["properties"]["C1"]["properties"]["evidence"]["items"]["anyOf"]
        self.assertEqual([c["properties"]["source"]["enum"][0] for c in citations], ["E1", "S1"])
        self.assertEqual(bodies[-1]["input"][1:3], self.tool_reply()["response"]["output"])

    def test_text_repair_keeps_failed_output_and_uses_one_complete_resubmission(self):
        for name, invalid, second, status in (
                ("missing", {"C1": self.values()["C1"]}, self.text_reply(), "valid"),
                ("bad-json", "not a decision", self.text_reply(), "valid"),
                ("twice-bad", {}, self.text_reply({}), "failed")):
            first = self.text_reply(invalid)
            if name == "bad-json":
                first["response"]["output"][0]["content"][0]["text"] = "{broken>"
            bodies = []

            def request(provider, body, *args, **kwargs):
                bodies.append(copy.deepcopy(body))
                return first if len(bodies) == 1 else second

            with mock.patch.object(review, "one_request", side_effect=request):
                meta = self.run_case(name, decision_contract=True, decision_text=True, repair_decision=True)
            self.assertEqual((meta["status"], meta["requests"], meta["repair_requests"], meta["max_requests"]),
                             (status, 2, 1, 2))
            self.assertNotIn("tools", bodies[-1])
            self.assertEqual(bodies[-1]["input"][1], first["response"]["output"][0])
            feedback = json.loads(bodies[-1]["input"][2]["content"])
            self.assertEqual(feedback["required_candidates"], ["C1", "C2"])
            self.assertTrue((self.root / name / "request-1/answer.txt").exists())

    def test_text_contract_rejects_functions_and_invalid_text_without_implicit_retry(self):
        for name, response, repair in (("function", self.submit(), True),
                                       ("legacy", self.final(), False),
                                       ("transport", {"event": "request.error"}, True)):
            with mock.patch.object(review, "one_request", return_value=response) as request:
                meta = self.run_case(name, decision_contract=True, decision_text=True, repair_decision=repair)
            self.assertEqual((meta["status"], meta["requests"], meta["repair_requests"]), ("failed", 1, 0))
            request.assert_called_once()
        with self.assertRaisesRegex(ValueError, "requires"):
            self.run_case("invalid-text-flag", decision_text=True)

    def test_valid_settled_and_unresolved_decisions_preserve_behavior(self):
        value = self.values()
        value["C1"].update(verdict="confirmed", severity="medium", basis="introduced_or_worsened")
        value["C2"].update(verdict="unresolved", basis="gap", evidence=[], missing="caller body",
                           comparison={key: "" for key in ("input", "before", "after")})
        self.assertEqual(self.parse(value), [{"id": ident, **row} for ident, row in value.items()])

    def test_missing_extra_and_duplicate_candidate_or_field_are_rejected(self):
        for mutate in (lambda v: v.pop("C2"), lambda v: v.update(C3=v["C1"]),
                       lambda v: v["C1"].pop("missing"), lambda v: v["C1"].update(extra=True),
                       lambda v: v["C1"]["comparison"].update(extra="extra")):
            value = self.values()
            mutate(value)
            with self.assertRaises(ValueError):
                self.parse(value)
        raw = json.dumps(self.values())
        for duplicate in (raw.replace('"C1":', '"C2":', 1),
                          raw.replace('"missing": ""', '"missing":"", "missing":""', 1)):
            with self.assertRaisesRegex(ValueError, "duplicate"):
                contract.parse_submission(duplicate, self.packet)

    def test_settled_decision_needs_both_behaviors_and_matching_basis(self):
        for mutate in (lambda r: r["comparison"].update(input=""),
                       lambda r: r["comparison"].update(before="  "),
                       lambda r: r["comparison"].update(after=None),
                       lambda r: r.update(basis="introduced_or_worsened"),
                       lambda r: r.update(basis=[]), lambda r: r.update(missing="unknown caller"),
                       lambda r: r.update(verdict="unresolved", basis="gap", missing="")):
            value = self.values()
            mutate(value["C1"])
            with self.assertRaises(ValueError):
                self.parse(value)

    def test_citations_keep_original_candidate_and_line_constraints(self):
        for citation in ({"source": "E2", "line": 1}, {"source": "S1", "line": 1},
                         {"source": "E1", "line": 3}, {"source": "E1", "line": True},
                         {"source": "E1", "line": 1, "extra": "value"}):
            value = self.values()
            value["C1"]["evidence"] = [citation]
            with self.assertRaises(ValueError):
                self.parse(value)

    def test_forced_submission_uses_one_request_and_keeps_sampling(self):
        bodies = []

        def request(provider, body, *args, **kwargs):
            bodies.append(copy.deepcopy(body))
            return self.submit()

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(decision_contract=True)
        self.assertEqual((meta["status"], meta["requests"], meta["tool_calls"]), ("valid", 1, 0))
        self.assertEqual(bodies[0]["tool_choice"], {"type": "function", "name": contract.NAME})
        self.assertEqual([tool["name"] for tool in bodies[0]["tools"]], [contract.NAME])
        self.assertFalse(bodies[0]["parallel_tool_calls"])
        self.assertEqual({k: bodies[0][k] for k in self.sampling()}, self.sampling())

    def test_last_round_only_submits_with_updated_candidate_citation_schema(self):
        bodies = []

        def request(provider, body, *args, **kwargs):
            bodies.append(copy.deepcopy(body))
            return self.tool_reply() if len(bodies) == 1 else self.submit("S1")

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(decision_contract=True, with_tools=True, max_requests=2)
        self.assertEqual((meta["status"], meta["requests"], meta["tool_calls"]), ("valid", 2, 1))
        self.assertEqual(bodies[0]["tool_choice"], "required")
        self.assertEqual([t["name"] for t in bodies[-1]["tools"]], [contract.NAME])
        schema = bodies[-1]["tools"][0]["parameters"]
        self.assertEqual(schema["required"], ["C1", "C2"])
        self.assertFalse(schema["additionalProperties"])
        for ident, expected in (("C1", ["E1", "S1"]), ("C2", ["E2"])):
            row = schema["properties"][ident]
            self.assertEqual(set(row["required"]), set(self.values()[ident]))
            citations = row["properties"]["evidence"]["items"]["anyOf"]
            self.assertEqual([c["properties"]["source"]["enum"][0] for c in citations], expected)
            self.assertEqual(citations[0]["properties"]["line"], {"type": "integer", "minimum": 1, "maximum": 2})
        self.assertEqual(bodies[-1]["input"][1:3], self.tool_reply()["response"]["output"])

    def test_plaintext_missing_fields_and_mixed_submission_fail_without_retry(self):
        invalid = self.submit()
        values = self.values()
        values["C1"].pop("missing")
        invalid["response"]["output"][0]["arguments"] = json.dumps(values)
        mixed = self.submit()
        mixed["response"]["output"].extend(self.tool_reply()["response"]["output"])
        repeated = self.submit()
        repeated["response"]["output"].append({**repeated["response"]["output"][0], "call_id": "submit-2"})
        unsupported = {"event": "request.error", "http_status": 400}
        for name, response in (("plain", self.final()), ("missing", invalid), ("mixed", mixed),
                               ("repeated", repeated), ("unsupported", unsupported)):
            with self.subTest(name=name), mock.patch.object(review, "one_request", return_value=response) as request:
                meta = self.run_case(name, decision_contract=True, with_tools=True)
            self.assertEqual((meta["status"], meta["requests"], meta["tool_calls"]), ("failed", 1, 0))
            request.assert_called_once()
        self.assertTrue((self.root / "missing/answer.txt").exists())
        self.assertTrue((self.root / "missing/evidence-packet.json").exists())

    def test_empty_candidates_do_not_build_a_submission_or_request(self):
        packet = review.make_packet({"name": "empty", "split": "unscored", "target": self.revisions[1],
                                     "candidates": []}, self.repo)
        with mock.patch.object(review, "one_request") as request, mock.patch.object(contract, "submission_tool") as tool:
            meta = evidence.investigate(packet, {}, {}, self.root / "absent", self.root / "empty",
                                        decision_contract=True, with_tools=True)
        request.assert_not_called()
        tool.assert_not_called()
        self.assertEqual((meta["status"], meta["requests"]), ("skipped-empty", 0))

    def test_one_explicit_repair_resubmits_the_whole_packet_without_sources(self):
        invalid = self.submit()
        values = self.values()
        values.pop("C2")
        invalid["response"]["output"][0]["arguments"] = json.dumps(values)
        valid = self.submit()
        valid["response"]["output"][0]["call_id"] = "submit-2"
        bodies = []

        def request(provider, body, *args, **kwargs):
            bodies.append(copy.deepcopy(body))
            return invalid if len(bodies) == 1 else valid

        with mock.patch.object(review, "one_request", side_effect=request):
            meta = self.run_case(decision_contract=True, repair_decision=True, with_tools=True)
        self.assertEqual((meta["status"], meta["requests"], meta["repair_requests"]), ("valid", 2, 1))
        self.assertEqual(meta["max_requests"], 9)
        self.assertEqual([t["name"] for t in bodies[-1]["tools"]], [contract.NAME])
        self.assertEqual(bodies[-1]["tool_choice"], {"type": "function", "name": contract.NAME})
        feedback = next(i for i in bodies[-1]["input"] if i.get("type") == "function_call_output")
        self.assertFalse(json.loads(feedback["output"])["whole_packet_accepted"])
        self.assertEqual(json.loads(feedback["output"])["required_candidates"], ["C1", "C2"])
        self.assertTrue((self.root / "case/request-1/answer.txt").exists())
        self.assertEqual(len(review.load_json(self.root / "case/verdicts.json")), 2)

    def test_invalid_json_can_be_repaired_but_second_failure_stops(self):
        bad = self.submit()
        bad["response"]["output"][0]["arguments"] = '{"broken>'
        for name, second, status in (("fixed", self.submit(), "valid"), ("still-bad", copy.deepcopy(bad), "failed")):
            second["response"]["output"][0]["call_id"] = "submit-2"
            with mock.patch.object(review, "one_request", side_effect=[bad, second]) as request:
                meta = self.run_case(name, decision_contract=True, repair_decision=True)
            self.assertEqual((meta["status"], meta["requests"], meta["repair_requests"]), (status, 2, 1))
            self.assertEqual(request.call_count, 2)

    def test_repair_does_not_retry_transport_or_accepted_decisions(self):
        for name, response, status in (("good", self.submit(), "valid"),
                                       ("transport", {"event": "request.error", "http_status": 400}, "failed"),
                                       ("plain", self.final(), "failed")):
            with mock.patch.object(review, "one_request", return_value=response) as request:
                meta = self.run_case(name, decision_contract=True, repair_decision=True)
            self.assertEqual((meta["status"], meta["requests"], meta["repair_requests"]), (status, 1, 0))
            request.assert_called_once()
        with self.assertRaisesRegex(ValueError, "requires"):
            self.run_case("invalid-flags", repair_decision=True)

    def test_real_child_http_accepts_submission_and_renders_comparison(self):
        received = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(handler):
                received.append(json.loads(handler.rfile.read(int(handler.headers["Content-Length"]))))
                response = self.submit()
                payload = ("data: " + json.dumps({"type": response["event"], "response": response["response"]}) + "\n\n").encode()
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
                    self.sampling(), self.repo, self.root / "http", decision_contract=True, cargo_home=self.cargo)
            finally:
                server.shutdown()
                thread.join()
        self.assertEqual(meta["status"], "valid")
        self.assertEqual(received[0]["tool_choice"]["name"], contract.NAME)
        import review_pipeline
        rows = review.load_json(self.root / "http/verdicts.json")
        report = review_pipeline.report_b("initial", rows, self.packet)
        self.assertIn("Before: guard prevents the claimed failure", report)
        self.assertIn("After: guard still prevents it", report)


if __name__ == "__main__":
    unittest.main()
