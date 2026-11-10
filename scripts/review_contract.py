"""Plan 278：有来源边界与前后行为比较的裁决提交契约。"""

import json

import review_recheck as review


NAME = "submit_review"
INSTRUCTIONS = '''
Final delivery uses submit_review, not a prose response or the legacy JSON
example in the evidence packet. Its function schema is the final format.
Call it exactly once, alone, when your decisions are ready. Every candidate key
and every field is required; missing is an empty string for a settled decision.

For both confirmed and excluded, give one concrete input/configuration/event
sequence and trace that SAME input through before and after. State the observed
result at the consumer, including valid inputs that may now be rejected. For an
exclusion, establish one of: the claimed failure is disproved, the SAME failure
already happened before and is unaffected, or the changed result is harmless.
Stricter checks, fail-closed behavior, a new document, or a new cleanup path do
not alone prove harmlessness, correctness, or a new regression. Check the caller
that consumes an unknown/empty/default value. If its behavior is missing, use
source tools or unresolved; do not invent it. For unresolved, state the precise
gap in missing and keep any unknown comparison fields empty.
'''

REPAIR_INSTRUCTIONS = '''
The submission failed local validation. You have one repair request, with no
source tools. Call submit_review alone with the COMPLETE set of candidate keys
and ALL required fields, including null severity and empty missing where needed.
Return valid JSON arguments matching the schema. Do not use internal markup.
Recheck the same-input comparison; use unresolved for facts still missing.
'''


def validation_feedback(error, packet):
    return {"status": "rejected", "error_type": type(error).__name__,
            "error": str(error), "required_candidates": [c["id"] for c in packet["candidates"]],
            "whole_packet_accepted": False}


def object_schema(properties):
    return {"type": "object", "properties": properties, "required": list(properties),
            "additionalProperties": False}


def submission_tool(packet):
    payload = json.loads(packet["prompt"][len(review.PROMPT):])
    sources = {row["id"]: row for row in packet["evidence"]}
    properties = {}
    for candidate in payload["candidates"]:
        citations = []
        for ident in candidate["evidence"]:
            source = sources[ident]
            citations.append(object_schema({"source": {"type": "string", "enum": [ident]},
                "line": {"type": "integer", "minimum": source["start"], "maximum": source["end"]}}))
        if not citations:
            raise ValueError("decision candidate has no authorized evidence")
        properties[candidate["id"]] = object_schema({
            "verdict": {"type": "string", "enum": ["confirmed", "excluded", "unresolved"]},
            "severity": {"type": ["string", "null"], "enum": ["high", "medium", "low", None]},
            "reason": {"type": "string"},
            "comparison": object_schema({key: {"type": "string"} for key in ("input", "before", "after")}),
            "basis": {"type": "string", "enum": ["introduced_or_worsened", "missing_fix_path",
                                                    "disproved", "preexisting", "harmless", "gap"]},
            "evidence": {"type": "array", "items": {"anyOf": citations}},
            "missing": {"type": "string"},
        })
    return {"type": "function", "name": NAME, "strict": True,
            "description": "Submit final decisions, one per candidate, with the same input traced before and after. Call alone; all fields are mandatory.",
            "parameters": object_schema(properties)}


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate property in decision submission")
        value[key] = item
    return value


def parse_submission(text, packet):
    if len(text.encode()) > 1 << 20:
        raise ValueError("decision submission exceeds one MiB")
    value = json.loads(text, object_pairs_hook=unique_object)
    expected = [c["id"] for c in packet["candidates"]]
    if not isinstance(value, dict) or set(value) != set(expected):
        raise ValueError("submission must contain exactly the candidate keys")
    rows = []
    for ident in expected:
        row = value[ident]
        fields = {"verdict", "severity", "reason", "comparison", "basis", "evidence", "missing"}
        if not isinstance(row, dict) or set(row) != fields:
            raise ValueError("decision has missing or extra fields")
        comparison = row["comparison"]
        if (not isinstance(comparison, dict) or set(comparison) != {"input", "before", "after"}
                or any(not isinstance(v, str) for v in comparison.values())):
            raise ValueError("decision needs a typed before/after comparison")
        bases = {"confirmed": {"introduced_or_worsened", "missing_fix_path"},
                 "excluded": {"disproved", "preexisting", "harmless"}, "unresolved": {"gap"}}
        if (not isinstance(row["verdict"], str) or not isinstance(row["basis"], str)
                or row["basis"] not in bases.get(row["verdict"], set())):
            raise ValueError("decision basis does not match verdict")
        if row["verdict"] != "unresolved":
            if any(not v.strip() for v in comparison.values()):
                raise ValueError("settled decision needs input and both behaviors")
            if row["missing"] != "":
                raise ValueError("settled decision must have empty missing")
        if not isinstance(row["evidence"], list):
            raise ValueError("decision evidence must be an array")
        for citation in row["evidence"]:
            if not isinstance(citation, dict) or set(citation) != {"source", "line"}:
                raise ValueError("citation has missing or extra fields")
        rows.append({"id": ident, **row})
    # 来源与行范围继续走旧校验，不因结构化输出降低验收标准。
    return review.parse_verdicts(json.dumps({"verdicts": rows}), packet)
