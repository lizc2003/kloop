"""函数参数的增量、完成条目与终态应独立核对。"""

import copy
import itertools
import json
import pathlib
import tempfile
import unittest

import review_stream as stream


class StreamTests(unittest.TestCase):
    def events(self, arguments='{"value":"完整"}'):
        item = {"id": "fc1", "type": "function_call", "call_id": "call1",
                "name": "submit", "arguments": arguments}
        return [
            {"type": "response.output_item.added", "item": {**item, "arguments": ""}},
            {"type": "response.function_call_arguments.delta", "item_id": "fc1", "delta": arguments[:5]},
            {"type": "response.function_call_arguments.delta", "item_id": "fc1", "delta": arguments[5:]},
            {"type": "response.function_call_arguments.done", "item_id": "fc1", "arguments": arguments},
            {"type": "response.output_item.done", "item": item},
            {"type": "response.completed", "response": {"status": "completed", "output": [item]}},
        ]

    def test_matching_stream_does_not_imply_valid_json(self):
        for arguments, valid in (('{"value":"完整"}', True), ('{"broken>', False)):
            result = stream.audit(self.events(arguments))
            self.assertEqual({"stream_consistent": result["stream_consistent"], "calls": result["calls"]},
                {"stream_consistent": True, "calls": [{"output_index": 0,
                    "argument_bytes": len(arguments.encode()), "arguments_json_valid": valid, "delta_count": 2,
                    "deltas_match_final": True, "argument_done_matches_final": True, "item_done_matches_final": True}]})

    def test_changed_missing_or_duplicated_representations_are_detected(self):
        for mutate in (lambda e: e[1].update(delta="lost"), lambda e: e[3].update(arguments="{}"),
                       lambda e: e[4].update(item={**e[4]["item"], "call_id": "other"}),
                       lambda e: e.pop(3), lambda e: e.insert(3, copy.deepcopy(e[3]))):
            events = self.events()
            mutate(events)
            self.assertFalse(stream.audit(events)["stream_consistent"])

    def test_orphan_delta_and_duplicate_final_ids_are_detected(self):
        events = self.events()
        events.insert(1, {"type": "response.function_call_arguments.delta", "item_id": "orphan", "delta": "{}"})
        self.assertFalse(stream.audit(events)["stream_consistent"])
        events = self.events()
        events[-1]["response"]["output"].append(copy.deepcopy(events[-1]["response"]["output"][0]))
        self.assertFalse(stream.audit(events)["stream_consistent"])

    def test_added_item_may_omit_the_initial_arguments(self):
        events = self.events()
        events[0]["item"].pop("arguments")
        self.assertTrue(stream.audit(events)["stream_consistent"])

    def test_initial_arguments_and_interleaved_calls_keep_their_own_fragments(self):
        first, second = self.events(), self.events('{"second":2}')
        first[0]["item"]["arguments"] = first[1]["delta"]
        first.pop(1)
        for event in second:
            if "item_id" in event:
                event["item_id"] = "fc2"
            if "item" in event:
                event["item"] = {**event["item"], "id": "fc2", "call_id": "call2"}
        terminal = first[-1]
        terminal["response"]["output"].append(second[-2]["item"])
        events = [event for pair in itertools.zip_longest(first[:-1], second[:-1])
                  for event in pair if event is not None] + [terminal]
        result = stream.audit(events)
        self.assertTrue(result["stream_consistent"])
        self.assertEqual([r["delta_count"] for r in result["calls"]], [1, 2])

    def test_missing_duplicate_or_nonfinal_terminal_is_rejected(self):
        for events in (self.events()[:-1], self.events() + [self.events()[-1]],
                       self.events() + [{"type": "response.created"}], []):
            with self.assertRaises(ValueError):
                stream.audit(events)

    def test_sse_reader_accepts_multiline_unicode_and_rejects_truncation(self):
        with tempfile.TemporaryDirectory() as root:
            path = pathlib.Path(root)/"response.sse"
            event = {"type": "response.created", "value": "完整"}
            path.write_text(": comment\r\ndata: " + json.dumps(event, ensure_ascii=False, indent=1).replace("\n", "\r\ndata: ")
                            + "\r\n\r\ndata: [DONE]\r\n\r\n")
            self.assertEqual(stream.read_events(path), [event])
            path.write_text('data: {"type":"response.created"}\n')
            with self.assertRaisesRegex(ValueError, "unfinished"):
                stream.read_events(path)
            path.write_text('data: []\n\n')
            with self.assertRaisesRegex(ValueError, "object"):
                stream.read_events(path)


if __name__ == "__main__":
    unittest.main()
