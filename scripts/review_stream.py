"""Plan 279：离线核对函数参数在 SSE 各表示之间是否一致，不修复内容。"""

import collections
import json
import pathlib


TERMINALS = {"response.completed", "response.incomplete", "response.failed"}


def read_events(path):
    events, pending, total = [], [], 0
    with pathlib.Path(path).open("rb") as stream:
        for raw in stream:
            total += len(raw)
            if total > 32 << 20:
                raise ValueError("SSE capture exceeds 32 MiB")
            line = raw.decode("utf-8").rstrip("\r\n")
            if line.startswith("data:"):
                pending.append(line[5:].lstrip(" "))
            elif not line and pending:
                data, pending = "\n".join(pending), []
                if data != "[DONE]":
                    event = json.loads(data)
                    if not isinstance(event, dict) or not isinstance(event.get("type"), str):
                        raise ValueError("invalid SSE event object")
                    events.append(event)
    if pending:
        raise ValueError("unfinished SSE event")
    return events


def audit(events):
    terminals = [e for e in events if e.get("type") in TERMINALS]
    if len(terminals) != 1 or events[-1] is not terminals[0]:
        raise ValueError("expected one terminal event at the end")
    terminal = terminals[0]
    output = terminal["response"]["output"]
    calls, final_ids = [], []
    for index, item in enumerate(output):
        if item.get("type") != "function_call":
            continue
        ident, arguments = item.get("id"), item.get("arguments")
        final_ids.append(ident)
        added = [e.get("item", {}) for e in events if e.get("type") == "response.output_item.added"
                 and e.get("item", {}).get("id") == ident]
        deltas = [e.get("delta") for e in events if e.get("type") == "response.function_call_arguments.delta"
                  and e.get("item_id") == ident]
        done = [e.get("arguments") for e in events if e.get("type") == "response.function_call_arguments.done"
                and e.get("item_id") == ident]
        items = [e.get("item", {}) for e in events if e.get("type") == "response.output_item.done"
                 and e.get("item", {}).get("id") == ident]
        initial = added[0].get("arguments", "") if len(added) == 1 else None
        delta_match = (isinstance(initial, str) and all(isinstance(d, str) for d in deltas)
                       and initial + "".join(deltas) == arguments)
        item_match = (len(items) == 1 and all(items[0].get(key) == item.get(key)
                      for key in ("type", "name", "call_id", "arguments")))
        try:
            json.loads(arguments)
            json_valid = True
        except (ValueError, TypeError):
            json_valid = False
        calls.append({"output_index": index, "argument_bytes": len(arguments.encode()) if isinstance(arguments, str) else None,
                      "arguments_json_valid": json_valid, "delta_count": len(deltas),
                      "deltas_match_final": delta_match,
                      "argument_done_matches_final": len(done) == 1 and done[0] == arguments,
                      "item_done_matches_final": item_match})
    argument_ids = [e.get("item_id") for e in events
                    if e.get("type", "").startswith("response.function_call_arguments.")]
    identities_match = (all(isinstance(i, str) and i for i in final_ids)
                        and len(set(final_ids)) == len(final_ids)
                        and all(i in final_ids for i in argument_ids))
    consistent = identities_match and all(all(row[key] for key in
        ("deltas_match_final", "argument_done_matches_final", "item_done_matches_final")) for row in calls)
    return {"terminal_event": terminal["type"], "terminal_status": terminal["response"].get("status"),
            "event_counts": dict(collections.Counter(e["type"] for e in events)),
            "stream_consistent": consistent, "calls": calls}


def command(args):
    result = audit(read_events(args.capture))
    # 只输出形状和一致性；不回显参数、推理、调用标识或其他原始捕获。
    print(json.dumps(result, ensure_ascii=False))
    if not result["stream_consistent"]:
        raise SystemExit(1)


def add_commands(sub):
    parser = sub.add_parser("stream-audit", help="compare raw SSE function arguments without repairing them")
    parser.add_argument("capture")
    parser.set_defaults(handler=command)
