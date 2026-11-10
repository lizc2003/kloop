"""Plan 276：在 benchmark 外层自动准备候选复核，不改变共享循环。"""

import argparse
import copy
import difflib
import http.server
import json
import os
import pathlib
import re
import shutil
import subprocess
import threading
import time
import tomllib

import review_recheck as recheck
import review_evidence


LEDGER_INSTRUCTIONS = '''

Benchmark output contract (in addition to the review above):
Finish the report with exactly one fenced `review-ledger` JSON block. Include
every investigated candidate, including excluded ones. Do not create candidates
just to populate the ledger. An empty list explicitly means none investigated.
Use this exact schema:
```review-ledger
{"version":1,"candidates":[{"id":"C1","question":"Could input X cause result Y?",
"verdict":"confirmed|excluded|unresolved","reason":"Your actual initial verdict reason",
"evidence":[{"version":"after","path":"repo/relative/file","line":123}]}]}
```
The question must be a neutral, self-contained hypothesis, without your verdict,
exclusion reason, author intent, or claims about whether it is established.
Do not omit a candidate because you excluded it. Downgraded defects are confirmed.
Each candidate needs source anchors for its trigger, caller, callee, and relevant
guards that you actually investigated. Anchor line numbers refer to the frozen
before/after Git blobs, not the diff or working copy. Include unchanged caller
files when needed. The harness will collect fixed windows around these anchors
and corresponding code in the other revision; you do not choose excerpt ranges.
No gold labels or later fixes are available to you.
'''

OPEN_S, IDLE_S, BODY_WALL_S = review_evidence.OPEN_S, review_evidence.IDLE_S, review_evidence.BODY_WALL_S


def parse_ledger(report):
    blocks = re.findall(r"^```review-ledger\s*\n(.*?)^```[ \t]*$", report, re.M | re.S)
    if len(blocks) != 1:
        raise ValueError("expected exactly one review-ledger block")
    ledger = json.loads(blocks[0])
    if ledger.get("version") != 1 or not isinstance(ledger.get("candidates"), list):
        raise ValueError("invalid candidate ledger")
    ids = set()
    for row in ledger["candidates"]:
        ident = row["id"]
        if not isinstance(ident, str) or not ident.strip() or ident in ids:
            raise ValueError("candidate ids must be unique nonempty strings")
        ids.add(ident)
        if row["verdict"] not in ("confirmed", "excluded", "unresolved"):
            raise ValueError("invalid initial verdict")
        for key in ("question", "reason"):
            if not isinstance(row[key], str) or not row[key].strip():
                raise ValueError(f"candidate needs {key}")
        if not isinstance(row["evidence"], list) or not row["evidence"]:
            raise ValueError("candidate needs source anchors")
        for anchor in row["evidence"]:
            path = anchor["path"]
            if (not isinstance(path, str) or not path or "\0" in path
                    or pathlib.PurePosixPath(path).is_absolute()
                    or ".." in pathlib.PurePosixPath(path).parts):
                raise ValueError("source path must be repository relative")
            if anchor["version"] not in ("before", "after"):
                raise ValueError("source version must be before or after")
            if type(anchor["line"]) is not int or anchor["line"] < 1:
                raise ValueError("source anchor must be a positive line number")
    return ledger


def seal(packet):
    packet["prompt_chars"] = len(packet["prompt"])
    packet["prompt_sha256"] = recheck.digest(packet["prompt"])
    packet.pop("packet_sha256", None)
    packet["packet_sha256"] = recheck.digest(json.dumps(packet, ensure_ascii=False, sort_keys=True))
    return packet


def automatic_packet(ledger, source, target, context=80, max_chars=160_000):
    """窗口是固定机械规则；模型只给锚点，人工不参与问题改写或行段选择。"""
    if type(context) is not int or context < 0:
        raise ValueError("context must be nonnegative")
    target = recheck.git(source, "rev-parse", "--verify", f"{target}^{{commit}}").strip()
    revisions = {"after": target, "before": recheck.git(source, "rev-parse", f"{target}^").strip()}
    cache = {}

    def lines(version, path):
        key = (version, path)
        if key not in cache:
            # ls-tree 区分确实缺文件和 git show 本身失败；不把所有读取错误当作不存在。
            present = recheck.git(source, "ls-tree", revisions[version], "--", f":(literal){path}")
            if present and present.split("\t", 1)[0].split()[1] != "blob":
                raise ValueError("source anchor must name a Git blob, not a directory or submodule")
            cache[key] = (recheck.git(source, "show", f"{revisions[version]}:{path}").splitlines()
                          if present else None)
        return cache[key]

    candidates, gaps = [], []
    for row in ledger["candidates"]:
        if row["verdict"] != "excluded":
            continue
        ranges = {}

        def add(version, path, start, end):
            content = lines(version, path)
            if not content:
                gaps.append({"candidate": row["id"], "version": version, "path": path,
                             "state": "absent" if content is None else "empty"})
                return
            lo, hi = max(1, start - context), min(len(content), end + context)
            ranges.setdefault((version, path), []).append((lo, hi))

        for anchor in row["evidence"]:
            version, path, number = (anchor[k] for k in ("version", "path", "line"))
            content = lines(version, path)
            if content is None or number > len(content):
                raise ValueError("initial review supplied an invalid frozen source anchor")
            add(version, path, number, number)
            other = "before" if version == "after" else "after"
            counterpart = lines(other, path)
            if not counterpart:
                add(other, path, 1, 1)
                continue
            # equal 映射对应行；替换/删除取整个对应块，插入点保留两侧邻行。
            for _, a0, a1, b0, b1 in difflib.SequenceMatcher(
                    None, content, counterpart, autojunk=False).get_opcodes():
                if a0 <= number - 1 < a1:
                    if content[a0:a1] == counterpart[b0:b1]:
                        start = end = b0 + number - a0
                    else:
                        start, end = max(1, b0), min(len(counterpart), b1 + 1)
                    add(other, path, start, end)
                    break
        specs = []
        for (version, path), spans in sorted(ranges.items()):
            merged = []
            for start, end in sorted(spans):
                if merged and start <= merged[-1][1] + 1:
                    merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
                else:
                    merged.append((start, end))
            specs.extend({"version": version, "path": path, "start": start, "end": end}
                         for start, end in merged)
        candidates.append({"id": row["id"], "question": row["question"], "expected": "unknown",
                           "original_reason": row["reason"], "evidence": specs})
    # 跨候选也合并重叠窗口，供不同问题引用同一段源码；保留的行集合不变。
    shared = {}
    for candidate in candidates:
        for spec in candidate["evidence"]:
            shared.setdefault((spec["version"], spec["path"]), []).append((spec["start"], spec["end"]))
    for key, spans in shared.items():
        merged = []
        for start, end in sorted(spans):
            if merged and start <= merged[-1][1] + 1:
                merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
            else:
                merged.append((start, end))
        shared[key] = merged
    for candidate in candidates:
        specs = []
        for spec in candidate["evidence"]:
            for start, end in shared[(spec["version"], spec["path"])]:
                if start <= spec["start"] and spec["end"] <= end:
                    normalized = {**spec, "start": start, "end": end}
                    if normalized not in specs:
                        specs.append(normalized)
                    break
        candidate["evidence"] = specs
    packet = recheck.make_packet({"name": "automatic-review", "split": "unscored",
                                 "target": target, "candidates": candidates}, source, max_chars)
    payload = json.loads(packet["prompt"][len(recheck.PROMPT):])
    payload["source_gaps"] = gaps
    packet["prompt"] = recheck.PROMPT + json.dumps(payload, ensure_ascii=False)
    packet["preparation"] = {"context_lines": context, "max_chars": max_chars,
                             "source_gaps": gaps, "ledger_sha256": recheck.digest(
                                 json.dumps(ledger, ensure_ascii=False, sort_keys=True))}
    if len(packet["prompt"]) > max_chars:
        raise ValueError("automatic packet exceeds character budget; no request sent")
    return seal(packet)


def replace_endpoint(text, provider, endpoint):
    expected = tomllib.loads(text)
    expected["providers"][provider]["base_url"] = endpoint
    lines = text.splitlines(keepends=True)
    for index, line in enumerate(lines):
        if not re.match(r'\s*(?:base_url|"base_url"|\'base_url\')\s*=', line):
            continue
        changed = "".join([*lines[:index], f"base_url = {json.dumps(endpoint)}\n", *lines[index + 1:]])
        try:
            if tomllib.loads(changed) == expected:
                return changed
        except tomllib.TOMLDecodeError:
            pass
    raise ValueError("could not replace only the selected provider endpoint")


def probe_events():
    message = {"type": "message", "id": "probe-message", "status": "completed", "role": "assistant",
               "content": [{"type": "output_text", "text": "probe complete"}]}
    events = [
        {"type": "response.created", "response": {"id": "probe", "status": "in_progress"}},
        {"type": "response.output_item.added", "output_index": 0,
         "item": {**message, "status": "in_progress", "content": []}},
        {"type": "response.content_part.added", "output_index": 0, "item_id": message["id"],
         "content_index": 0, "part": {"type": "output_text", "text": ""}},
        {"type": "response.output_text.delta", "output_index": 0, "item_id": message["id"],
         "content_index": 0, "delta": "probe complete"},
        {"type": "response.output_text.done", "output_index": 0, "item_id": message["id"],
         "content_index": 0, "text": "probe complete"},
        {"type": "response.content_part.done", "output_index": 0, "item_id": message["id"],
         "content_index": 0, "part": {"type": "output_text", "text": "probe complete"}},
        {"type": "response.output_item.done", "output_index": 0, "item": message},
        {"type": "response.completed", "response": {"id": "probe", "status": "completed",
         "output": [message], "usage": {"input_tokens": 100, "output_tokens": 2, "total_tokens": 102}}},
    ]
    return "".join("data: " + json.dumps(event) + "\n\n" for event in events).encode()


def capture_route(args, bench, out):
    """本机只接收请求，不转发；key 不记录，临时配置退出时删除。"""
    out.mkdir()
    home, workspace = out / "home", out / "workspace"
    state = home / ".kloop"
    state.mkdir(parents=True, mode=0o700)
    workspace.mkdir()
    config_path = pathlib.Path(args.config).expanduser().resolve()
    config = tomllib.loads(config_path.read_text())
    name = config["provider"]
    if config["providers"][name].get("wire_api") != "responses":
        raise ValueError("automatic recheck currently requires a Responses provider")
    text = bench.prepare_config(config_path.parent, state, None, config_path)
    received = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            payload = probe_events()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *args):
            pass

    temporary = state / "config.toml"
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            text = replace_endpoint(text, name, f"http://127.0.0.1:{server.server_port}")
            temporary.touch(mode=0o600)
            temporary.write_text(text)
            with (out / "events.ndjson").open("w") as stdout, (out / "stderr.txt").open("w") as stderr:
                result = subprocess.run([args.kloop, "--headless", "--json", "--max-rounds", "1",
                                         "Report probe complete."], cwd=workspace,
                                        env=dict(os.environ, HOME=str(home)), stdin=subprocess.DEVNULL,
                                        stdout=stdout, stderr=stderr, timeout=60)
        finally:
            temporary.unlink(missing_ok=True)
            server.shutdown()
            thread.join()
    recheck.save_json(out / "summary.json", {"requests": len(received), "exit": result.returncode,
                                           "only_endpoint_changed": True,
                                           "temporary_config_removed": not temporary.exists()})
    if result.returncode or len(received) != 1:
        raise ValueError("runtime request probe failed or sent more than one request")
    ignored = {"input", "instructions", "tools", "tool_choice", "parallel_tool_calls", "prompt_cache_key"}
    allowed = {"model", "reasoning", "max_output_tokens", "temperature", "top_p", "text", "include",
               "store", "stream"}
    if received[0].keys() - ignored - allowed:
        raise ValueError("runtime request contains unsupported sampling fields")
    body = {key: value for key, value in received[0].items() if key in allowed}
    if body.get("model") != config["providers"][name]["model"] or not body.get("stream"):
        raise ValueError("runtime route did not match the selected streaming model")
    recheck.save_json(out / "sampling.json", body)
    return name, body


def independent_request(packet, provider, sampling, out):
    out.mkdir()
    body = {**copy.deepcopy(sampling), "input": [{"role": "user", "content": packet["prompt"]}]}
    recheck.save_json(out / "request.json", body)
    meta = {"status": "failed", "requests": 1, "open_timeout_s": OPEN_S,
            "idle_timeout_s": IDLE_S, "body_wall_timeout_s": BODY_WALL_S,
            "usage": None, "prompt_sha256": packet["prompt_sha256"]}
    started = time.monotonic()
    try:
        result = recheck.one_request(provider, body, OPEN_S + BODY_WALL_S + 5,
                                     open_timeout=OPEN_S, idle_timeout=IDLE_S,
                                     body_wall_timeout=BODY_WALL_S, capture=out / "response.sse")
        recheck.save_json(out / "response.json", result)
        response = result.get("response") or {}
        meta["usage"] = response.get("usage")
        if result.get("event") != "response.completed" or response.get("status") != "completed":
            meta["transport_error"] = {key: result[key] for key in
                                       ("error_type", "http_status", "reason_type", "errno") if key in result}
            raise ValueError("independent response did not complete")
        answer = recheck.response_text(response)
        (out / "answer.txt").write_text(answer)
        rows = recheck.parse_verdicts(answer, packet)
        recheck.save_json(out / "verdicts.json", rows)
        meta["status"] = "valid"
    except (ValueError, KeyError, TypeError, AttributeError, subprocess.SubprocessError) as error:
        meta["error_type"] = type(error).__name__
    meta["wall_s"] = round(time.monotonic() - started, 3)
    recheck.save_json(out / "meta.json", meta)
    return meta


def report_b(report, rows, packet):
    text = report + "\n\nIndependent recheck (unscored; citations mechanically validated):\n\n"
    sources = {source["id"]: source for source in packet["evidence"]}
    for row in rows:
        citations = ", ".join(f"{sources[c['source']]['version']} {sources[c['source']].get('package', '')} {sources[c['source']]['path']}:{c['line']}"
                              for c in row["evidence"])
        severity = f" / {row['severity']}" if row["severity"] else ""
        text += f"- {row['id']} — {row['verdict']}{severity}: {row['reason']} ({citations})\n"
        if row["missing"]:
            text += f"  Missing evidence: {row['missing']}\n"
    return text


def finish(args, out, protocol, report, ledger, meta):
    """恢复入口复用相同规则和初审；不再发送一次初审或修改原失败记录。"""
    recheck.save_json(out / "ledger.json", ledger)
    meta["candidate_counts"] = {verdict: sum(c["verdict"] == verdict for c in ledger["candidates"])
                                for verdict in ("confirmed", "excluded", "unresolved")}
    meta["stage"] = "evidence"
    preparation_started = time.monotonic()
    packet = automatic_packet(ledger, pathlib.Path(args.source).resolve(), protocol["target"],
                              protocol["context_lines"], protocol["max_chars"])
    recheck.save_json(out / "packet.json", packet)
    (out / "recheck-prompt.txt").write_text(packet["prompt"])
    meta["evidence_wall_s"] = round(time.monotonic() - preparation_started, 3)
    meta["stage"] = "recheck"
    if packet["candidates"]:
        config_path = pathlib.Path(args.config).expanduser().resolve()
        config_text = config_path.read_text()
        if recheck.digest(config_text) != protocol["config_sha256"]:
            raise ValueError("configuration changed since initial review")
        provider = tomllib.loads(config_text)["providers"][protocol["provider"]]
        meta["recheck_requests"] = 1
        if protocol.get("evidence_tools", False):
            result = review_evidence.investigate(packet, provider, protocol["sampling"], args.source,
                out / "recheck", with_tools=True, max_requests=protocol["evidence_requests"],
                max_calls=protocol["evidence_calls"], max_chars=protocol["evidence_chars"])
            meta["recheck_requests"] = result["requests"]
        else:
            result = independent_request(packet, provider, protocol["sampling"], out / "recheck")
        meta["recheck_status"] = result["status"]
        if result["status"] != "valid":
            raise ValueError("independent recheck did not deliver valid verdicts")
        rows = recheck.load_json(out / "recheck/verdicts.json")
        extended = out / "recheck/evidence-packet.json"
        report_packet = recheck.load_json(extended) if extended.exists() else packet
        (out / "report-b.md").write_text(report_b(report, rows, report_packet))
    else:
        meta["recheck_status"] = "skipped-empty"
        (out / "report-b.md").write_text(report)
    meta["status"], meta["stage"] = "complete", "complete"


def run(args, bench):
    out = pathlib.Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    started = time.monotonic()
    meta = {"status": "failed", "stage": "prepare", "recheck_requests": 0}
    source = pathlib.Path(args.source).resolve()
    target = recheck.git(source, "rev-parse", "--verify", f"{args.commit}^{{commit}}").strip()
    config_path = pathlib.Path(args.config).expanduser().resolve()
    config_digest = recheck.digest(config_path.read_text())
    try:
        if args.context < 0 or args.max_chars < 1 or args.max_rounds < 1 or args.timeout <= 0:
            raise ValueError("invalid pipeline budget")
        if getattr(args, "evidence_tools", False) and (
                getattr(args, "evidence_requests", 8) < 2 or getattr(args, "evidence_calls", 64) < 1
                or getattr(args, "evidence_chars", 160_000) < 1):
            raise ValueError("invalid evidence follow-up budget")
        if not hasattr(recheck.signal, "setitimer"):
            raise ValueError("pipeline body deadline requires Unix interval timers")
        executable = shutil.which(args.kloop)
        if executable is None:
            raise ValueError("kloop executable not found")
        args.kloop = str(pathlib.Path(executable).resolve())
        meta["stage"] = "runtime-probe"
        name, sampling = capture_route(args, bench, out / "runtime-probe")
        skill = pathlib.Path(args.skill).read_text() + LEDGER_INSTRUCTIONS
        (out / "initial-skill.md").write_text(skill)
        protocol = {"version": 1, "target": target,
                    "baseline": recheck.git(source, "rev-parse", f"{target}^").strip(),
                    "sampling": sampling, "provider": name, "context_lines": args.context,
                    "max_chars": args.max_chars, "retries": 0, "initial_trials": 1,
                    "recheck_trials": 1, "initial_timeout_s": args.timeout,
                    "initial_max_rounds": args.max_rounds, "open_timeout_s": OPEN_S,
                    "idle_timeout_s": IDLE_S, "body_wall_timeout_s": BODY_WALL_S,
                    "evidence_tools": getattr(args, "evidence_tools", False),
                    "evidence_requests": getattr(args, "evidence_requests", 8),
                    "evidence_calls": getattr(args, "evidence_calls", 64),
                    "evidence_chars": getattr(args, "evidence_chars", 160_000),
                    "skill_sha256": recheck.digest(skill), "config_sha256": config_digest}
        recheck.save_json(out / "protocol.json", protocol)
        if recheck.digest(config_path.read_text()) != config_digest:
            raise ValueError("configuration changed during runtime probe")
        meta["stage"] = "initial-review"
        bench.run(argparse.Namespace(out=str(out / "initial"), commit=target, source=str(source),
                                     skill=str(out / "initial-skill.md"), system_prompt=None,
                                     config=str(config_path), kloop=args.kloop,
                                     max_rounds=args.max_rounds, timeout=args.timeout))
        initial = bench.stats_one(out / "initial")
        recheck.save_json(out / "initial-stats.json", initial)
        if initial.get("exit") != 0:
            raise ValueError("initial review did not complete")
        report = (out / "initial/report.md").read_text()
        (out / "report-a.md").write_text(report)
        meta["stage"] = "candidate-ledger"
        ledger = parse_ledger(report)
        finish(args, out, protocol, report, ledger, meta)
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        meta["error_type"] = type(error).__name__
        if isinstance(error, ValueError) and meta["stage"] in ("candidate-ledger", "evidence"):
            meta["error_detail"] = str(error)
    finally:
        meta["wall_s"] = round(time.monotonic() - started, 3)
        recheck.save_json(out / "meta.json", meta)
    print(json.dumps(meta, ensure_ascii=False), flush=True)
    if meta["status"] != "complete":
        raise SystemExit(1)


def resume(args):
    base, out = pathlib.Path(args.initial).resolve(), pathlib.Path(args.out).resolve()
    initial_meta = recheck.load_json(base / "meta.json")
    if initial_meta.get("recheck_requests") != 0:
        raise ValueError("a recheck was already attempted; resume does not retry requests")
    if recheck.load_json(base / "initial/meta.json")["exit"] != 0:
        raise ValueError("cannot resume an incomplete initial review")
    protocol = recheck.load_json(base / "protocol.json")
    report = (base / "report-a.md").read_text()
    ledger = parse_ledger(report)
    if report != (base / "initial/report.md").read_text() or ledger != recheck.load_json(base / "ledger.json"):
        raise ValueError("saved initial review or candidate ledger changed")
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    started = time.monotonic()
    meta = {"status": "failed", "stage": "evidence", "recheck_requests": 0,
            "resumed_from": str(base), "initial_and_failed_preparation_wall_s": initial_meta["wall_s"]}
    recheck.save_json(out / "protocol.json", protocol)
    (out / "report-a.md").write_text(report)
    try:
        finish(args, out, protocol, report, ledger, meta)
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        meta["error_type"] = type(error).__name__
        if isinstance(error, ValueError) and meta["stage"] in ("candidate-ledger", "evidence"):
            meta["error_detail"] = str(error)
    finally:
        meta["resume_wall_s"] = round(time.monotonic() - started, 3)
        meta["wall_s"] = round(initial_meta["wall_s"] + meta["resume_wall_s"], 3)
        recheck.save_json(out / "meta.json", meta)
    print(json.dumps(meta, ensure_ascii=False), flush=True)
    if meta["status"] != "complete":
        raise SystemExit(1)


def add_commands(sub, source, bench):
    parser = sub.add_parser("pipeline", help="initial review, automatic frozen evidence, independent recheck")
    parser.add_argument("out")
    parser.add_argument("--commit", required=True)
    parser.add_argument("--source", default=str(source))
    parser.add_argument("--skill", default=str(source / "rust/crates/core/src/skills/code-review/SKILL.md"))
    parser.add_argument("--config", default="~/.kloop/config.toml")
    parser.add_argument("--kloop", default="kloop")
    parser.add_argument("--max-rounds", type=int, default=80)
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument("--context", type=int, default=80)
    parser.add_argument("--max-chars", type=int, default=160_000)
    parser.add_argument("--evidence-tools", action="store_true", help="enable bounded frozen source follow-up")
    parser.add_argument("--evidence-requests", type=int, default=8)
    parser.add_argument("--evidence-calls", type=int, default=64)
    parser.add_argument("--evidence-chars", type=int, default=160_000)
    parser.set_defaults(handler=lambda args: run(args, bench))
    parser = sub.add_parser("pipeline-recheck", help="resume evidence preparation without repeating initial review")
    parser.add_argument("initial")
    parser.add_argument("out")
    parser.add_argument("--source", default=str(source))
    parser.add_argument("--config", default="~/.kloop/config.toml")
    parser.set_defaults(handler=resume)
