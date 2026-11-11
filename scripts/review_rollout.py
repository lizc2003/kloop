"""Plan 281：复核证据改用初审在 rollout 里实际读过的行，替代固定窗口。"""

import json
import os
import pathlib
import re
import subprocess
import time
import tomllib

import review_recheck as recheck
import review_pipeline as pipeline


MERGE_GAP = 3
PATCH_FILE = re.compile(r"^diff --git a/(\S+) b/(\S+)$")
PATCH_NEW = re.compile(r"^\+\+\+ (?:b/(\S+)|/dev/null)$")
PATCH_OLD = re.compile(r"^--- (?:a/(\S+)|/dev/null)$")
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
SED_RANGE = re.compile(r"""^sed\s+-n\s+['"]?(\d+)(?:,(\d+))?p['"]?(?:\s+(\S+))?$""")
SHOW_BLOB = re.compile(r"^git\s+show\s+(\S+?):(\S+)$")
GREP_LINE = re.compile(r"^(?:(?P<rev>[0-9a-fA-F]{7,40}|HEAD[\^~0-9]*):)?(?P<path>[^:\s][^:\n]*):(?P<line>\d+):")
NUMBERED = re.compile(r"^(\d+)\t")
# bash 内联截断说明（tools/bash.rs `format_output`）与会话落盘预览的结尾（history.rs `POINTER_END`）。
BASH_TRUNCATION = "\n[output truncated: "
OFFLOAD_POINTER_END = "so only what you extract enters the context]"
NL_PREFIX = re.compile(r"^\s*\d+\t")


def split_unquoted(text, separators):
    """按不在引号内的分隔符切分；分隔符按给定顺序匹配（长的放前面）。"""
    parts, current, quote, index = [], "", None, 0
    while index < len(text):
        char = text[index]
        if quote:
            current += char
            if char == quote:
                quote = None
            elif char == "\\" and quote == '"' and index + 1 < len(text):
                current += text[index + 1]
                index += 1
            index += 1
            continue
        if char in "'\"":
            quote = char
            current += char
            index += 1
            continue
        matched = next((s for s in separators if text.startswith(s, index)), None)
        if matched:
            parts.append(current.strip())
            current = ""
            index += len(matched)
            continue
        current += char
        index += 1
    parts.append(current.strip())
    return [part for part in parts if part]


def unquote(word):
    return word[1:-1] if len(word) >= 2 and word[0] == word[-1] and word[0] in "'\"" else word


class Reads:
    """初审快照里的读取记录；版本与行号都对照冻结的原仓库校验。"""

    def __init__(self, snapshot, source, revisions):
        self.snapshot = pathlib.Path(os.path.realpath(snapshot))
        self.source = pathlib.Path(source)
        self.revisions = revisions  # {"before": sha, "after": sha}，原仓库中的冻结提交
        head = recheck.git(self.snapshot, "rev-parse", "HEAD").strip()
        parent = recheck.git(self.snapshot, "rev-parse", "HEAD^").strip()
        self.snapshot_versions = {head: "after", parent: "before"}
        self.lines, self.first_seen, self.counts = {}, {}, {}
        self._files = {}

    def version_of(self, rev):
        try:
            sha = recheck.git(self.snapshot, "rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}").strip()
        except subprocess.CalledProcessError:
            return None
        return self.snapshot_versions.get(sha)

    def relative(self, path, base):
        path = unquote(path)
        full = pathlib.Path(os.path.realpath(path if os.path.isabs(path) else os.path.join(base, path)))
        try:
            return full.relative_to(self.snapshot).as_posix()
        except ValueError:
            return None

    def file_lines(self, version, path):
        key = (version, path)
        if key not in self._files:
            try:
                kind = recheck.git(self.source, "cat-file", "-t", f"{self.revisions[version]}:{path}").strip()
                text = recheck.git(self.source, "show", f"{self.revisions[version]}:{path}") if kind == "blob" else None
            except (subprocess.CalledProcessError, UnicodeDecodeError):
                text = None
            self._files[key] = None if text is None else text.splitlines()
        return self._files[key]

    def add(self, version, path, numbers, kind):
        if version is None or path is None:
            return
        content = self.file_lines(version, path)
        if not content:
            return
        valid = {n for n in numbers if 1 <= n <= len(content)}
        if not valid:
            return
        self.lines.setdefault((version, path), set()).update(valid)
        self.first_seen.setdefault((version, path), len(self.first_seen))
        self.counts[kind] = self.counts.get(kind, 0) + len(valid)

    def add_whole(self, version, path, kind):
        content = self.file_lines(version, path) if version and path else None
        if content:
            self.add(version, path, range(1, len(content) + 1), kind)

    def patch(self, output):
        old = new = None
        before = after = None
        for line in output.splitlines():
            if match := PATCH_FILE.match(line):
                old, new, before = match.group(1), match.group(2), None
                continue
            if match := PATCH_OLD.match(line):
                old, before = match.group(1), None
                continue
            if match := PATCH_NEW.match(line):
                new, before = match.group(1), None
                continue
            if match := HUNK.match(line):
                before, after = int(match.group(1)), int(match.group(3))
                continue
            if before is None:
                continue
            if line.startswith(" "):
                self.add("before", old, [before], "patch")
                self.add("after", new, [after], "patch")
                before, after = before + 1, after + 1
            elif line.startswith("-"):
                self.add("before", old, [before], "patch")
                before += 1
            elif line.startswith("+"):
                self.add("after", new, [after], "patch")
                after += 1
            elif not line.startswith("\\"):
                before = None

    def grep_output(self, output, base, single_file=None):
        for line in output.splitlines():
            match = GREP_LINE.match(line)
            if match:
                version = self.version_of(match.group("rev")) if match.group("rev") else "after"
                path = match.group("path")
                path = path if match.group("rev") else self.relative(path, base)
                self.add(version, path, [int(match.group("line"))], "grep")
            elif single_file and (number := re.match(r"^(\d+):", line)):
                self.add("after", single_file, [int(number.group(1))], "grep")

    def bash(self, command, output):
        # 整份结果被会话换成落盘预览时，模型只看到首尾片段，哪一行都无法确认读过。
        if output.rstrip().endswith(OFFLOAD_POINTER_END):
            self.counts["offloaded-skipped"] = self.counts.get("offloaded-skipped", 0) + 1
            return
        shown = output.partition(BASH_TRUNCATION)[0]
        base = str(self.snapshot)
        segments = split_unquoted(command, ["&&", "||", ";", "\n"])
        reads = []  # (version, path, start, end, kind, numbered)；end 为 None 表示整个文件
        for segment in segments:
            if segment.startswith("cd "):
                target = unquote(segment[3:].strip())
                base = os.path.realpath(target if os.path.isabs(target) else os.path.join(base, target))
                continue
            stages = split_unquoted(segment, ["|"])
            first = stages[0]
            sed = next((SED_RANGE.match(stage) for stage in stages[1:] if SED_RANGE.match(stage)), None)
            # 管道里任何一段是 `nl` 时输出带行号前缀，比较前去掉。
            numbered = any(stage == "nl" or stage.startswith("nl ") for stage in stages)
            if match := SED_RANGE.match(first):
                start, end, path = int(match.group(1)), int(match.group(2) or match.group(1)), match.group(3)
                if path:
                    reads.append(("after", self.relative(path, base), start, end, "sed", numbered))
            elif match := SHOW_BLOB.match(first):
                version, path = self.version_of(match.group(1)), unquote(match.group(2))
                if sed:
                    reads.append((version, path, int(sed.group(1)), int(sed.group(2) or sed.group(1)),
                                  "show-blob", numbered))
                elif len(stages) == 1:
                    reads.append((version, path, 1, None, "show-blob", False))
            elif first.startswith("nl ") and sed:
                reads.append(("after", self.relative(first.split()[-1], base),
                              int(sed.group(1)), int(sed.group(2) or sed.group(1)), "nl", True))
            elif first.startswith("cat ") and len(stages) == 1 and len(first.split()) == 2:
                reads.append(("after", self.relative(first.split()[1], base), 1, None, "cat", False))
        commands = [segment for segment in segments if not segment.startswith("cd ")]
        # 命令只说明想读哪里；是否真的显示要看输出。按命令顺序在输出里找每个读取对应
        # 冻结行从第一行起的最长连续前缀，只计这一段：截断、`| head`、`| grep`、没执行
        # 的条件分支都由此排除。顺序匹配让两个文件里相同的一行不会被算到错误的文件上。
        output_lines = shown.splitlines()
        cursor = 0
        for version, path, start, end, kind, numbered in reads:
            content = self.file_lines(version, path) if version and path else None
            if not content or start > len(content):
                continue
            expected = content[start - 1:(len(content) if end is None else min(end, len(content)))]
            lines = [NL_PREFIX.sub("", line, count=1) for line in output_lines] if numbered else output_lines
            position, length = longest_shown_prefix(expected, lines, cursor)
            if length:
                self.add(version, path, range(start, start + length), kind)
                cursor = position + length
            if length < len(expected):
                self.counts["unshown"] = self.counts.get("unshown", 0) + len(expected) - length
        single = None
        if len(commands) == 1 and len(split_unquoted(commands[0], ["|"])) == 1:
            words = commands[0].split()
            if words and words[0] in ("grep", "rg") and any(w.startswith("-") and "n" in w for w in words[1:]):
                path = self.relative(words[-1], base)
                single = path if path and self.file_lines("after", path) else None
        self.patch(shown)
        self.grep_output(shown, base, single)

    def read_file(self, arguments, output):
        path = self.relative(arguments.get("path", ""), str(self.snapshot))
        numbers = [int(m.group(1)) for line in output.splitlines() if (m := NUMBERED.match(line))]
        self.add("after", path, numbers, "read_file")

    def grep_tool(self, arguments, output):
        target = self.relative(arguments.get("path", "."), str(self.snapshot))
        single = target if target and self.file_lines("after", target) else None
        self.grep_output(output, str(self.snapshot), single)


def longest_shown_prefix(expected, lines, start=0):
    """`expected` 从第一行起在 `lines[start:]` 里连续出现的最长前缀：返回（位置，长度）。"""
    best = (start, 0)
    for index in range(start, len(lines)):
        if lines[index] != expected[0]:
            continue
        length = 0
        while (length < len(expected) and index + length < len(lines)
               and lines[index + length] == expected[length]):
            length += 1
        if length > best[1]:
            best = (index, length)
            if length == len(expected):
                break
    return best


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(item.get("text", "") for item in content if isinstance(item, dict))
    return ""


def read_regions(rollout, snapshot, source, revisions):
    reads = Reads(snapshot, source, revisions)
    calls = {}
    for line in pathlib.Path(rollout).read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") != "message":
            continue
        for block in event.get("content", []):
            if block.get("type") == "tool_use":
                calls[block.get("id")] = block
            elif block.get("type") == "tool_result" and block.get("tool_use_id") in calls:
                call, output = calls[block["tool_use_id"]], text_of(block.get("content"))
                arguments = call.get("input") or {}
                if call.get("name") == "bash":
                    reads.bash(arguments.get("command", ""), output)
                elif call.get("name") == "read_file":
                    reads.read_file(arguments, output)
                elif call.get("name") == "grep":
                    reads.grep_tool(arguments, output)
    return reads


def spans_of(numbers):
    spans = []
    for number in sorted(numbers):
        if spans and number <= spans[-1][1] + MERGE_GAP + 1:
            spans[-1][1] = max(spans[-1][1], number)
        else:
            spans.append([number, number])
    return [(start, end) for start, end in spans]


def rollout_packet(ledger, reads, source, target, max_chars):
    excluded = [row for row in ledger["candidates"] if row["verdict"] == "excluded"]
    anchored = {anchor["path"] for row in excluded for anchor in row["evidence"]}
    ordered = sorted(reads.lines, key=lambda key: (key[1] not in anchored, reads.first_seen[key]))
    spans = [(version, path, start, end) for version, path in ordered
             for start, end in spans_of(reads.lines[(version, path)])]

    def size(selected):
        """与 make_packet 生成的提示等长：同样的字段、顺序和 E 编号。"""
        evidence = []
        for index, (version, path, start, end) in enumerate(selected, 1):
            lines = reads.file_lines(version, path)
            evidence.append({"id": f"E{index}", "version": version, "revision": reads.revisions[version],
                             "path": path, "start": start, "end": end,
                             "text": "\n".join(f"{n}: {lines[n - 1]}" for n in range(start, end + 1))})
        ids = [row["id"] for row in evidence]
        candidates = [{"id": row["id"], "question": row["question"], "evidence": ids} for row in excluded]
        return len(recheck.PROMPT) + len(json.dumps({"candidates": candidates, "evidence": evidence},
                                                    ensure_ascii=False))

    def build(selected):
        specs = [{"version": v, "path": p, "start": s, "end": e} for v, p, s, e in selected]
        candidates = [{"id": row["id"], "question": row["question"], "expected": "unknown",
                       "original_reason": row["reason"], "evidence": specs} for row in excluded]
        return recheck.make_packet({"name": "rollout-evidence", "split": "unscored",
                                    "target": target, "candidates": candidates}, source, max_chars)

    # 整段按优先级取，不截断段；放不下的段记作丢弃，继续尝试后面更短的段。
    kept, dropped = [], []
    for span in spans:
        if size(kept + [span]) <= max_chars:
            kept.append(span)
        else:
            dropped.append(span)
    if excluded and not kept:
        raise ValueError("no read source fits the character budget")
    packet = build(kept)
    packet["preparation"] = {
        "evidence_source": "rollout", "max_chars": max_chars, "merge_gap": MERGE_GAP,
        "read_counts": reads.counts, "spans_total": len(spans), "spans_kept": len(kept),
        "dropped": [{"version": v, "path": p, "start": s, "end": e} for v, p, s, e in dropped],
        "ledger_sha256": recheck.digest(json.dumps(ledger, ensure_ascii=False, sort_keys=True)),
    }
    return pipeline.seal(packet)


def command(args):
    base, out = pathlib.Path(args.pipeline).resolve(), pathlib.Path(args.out).resolve()
    base_meta = recheck.load_json(base / "meta.json")
    # 配对只要求 A 组已经有结果：发过请求、无候选跳过，或账本已保存后在证据准备阶段失败
    # （例如固定窗口超预算）。A 组未交付也要跑 B 组，才能比交付。
    attempted = (base_meta.get("recheck_requests", 0) >= 1
                 or base_meta.get("recheck_status") == "skipped-empty"
                 or (base_meta.get("stage") == "evidence" and (base / "ledger.json").exists()))
    if not attempted:
        raise ValueError("the paired fixed-window recheck has not been attempted")
    attempt = base / "rollout-recheck-attempt.json"
    if attempt.exists():
        raise ValueError("a rollout recheck was already attempted; it is not retried")
    protocol = recheck.load_json(base / "protocol.json")
    ledger = recheck.load_json(base / "ledger.json")
    rollouts = sorted((base / "initial/home/.kloop/projects").glob("*/*/sessions/*.jsonl"))
    if len(rollouts) != 1:
        raise ValueError("expected exactly one initial review rollout")
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    started = time.monotonic()
    meta = {"status": "failed", "stage": "evidence", "recheck_requests": 0, "pipeline": str(base)}
    try:
        source = pathlib.Path(args.source).resolve()
        revisions = {"before": protocol["baseline"], "after": protocol["target"]}
        reads = read_regions(rollouts[0], base / "initial/repo", source, revisions)
        packet = rollout_packet(ledger, reads, source, protocol["target"], protocol["max_chars"])
        recheck.save_json(out / "packet.json", packet)
        (out / "recheck-prompt.txt").write_text(packet["prompt"])
        meta["preparation"] = {k: v for k, v in packet["preparation"].items() if k != "dropped"}
        meta["dropped_spans"] = len(packet["preparation"]["dropped"])
        meta["stage"] = "recheck"
        if not packet["candidates"]:
            meta.update(status="complete", recheck_status="skipped-empty")
        else:
            config_text = pathlib.Path(args.config).expanduser().resolve().read_text()
            if recheck.digest(config_text) != protocol["config_sha256"]:
                raise ValueError("configuration changed since initial review")
            provider = tomllib.loads(config_text)["providers"][protocol["provider"]]
            with attempt.open("x") as marker:
                json.dump({"out": str(out)}, marker)
            meta["recheck_requests"] = 1
            # 与 A 组同一套交付方式与补查设置，两组只差证据来源。
            result = pipeline.dispatch_recheck(protocol, packet, provider, source, out / "recheck")
            meta["recheck_requests"] = result.get("requests", 1)
            meta["recheck_status"] = result["status"]
            if result["status"] != "valid":
                raise ValueError("rollout recheck did not deliver valid verdicts")
            rows = recheck.load_json(out / "recheck/verdicts.json")
            extended = out / "recheck/evidence-packet.json"
            report_packet = recheck.load_json(extended) if extended.exists() else packet
            report = (base / "report-a.md").read_text()
            (out / "report-b.md").write_text(pipeline.report_b(report, rows, report_packet))
            meta["status"] = "complete"
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        meta["error_type"] = type(error).__name__
        if isinstance(error, ValueError):
            meta["error_detail"] = str(error)
    finally:
        meta["wall_s"] = round(time.monotonic() - started, 3)
        recheck.save_json(out / "meta.json", meta)
    print(json.dumps({k: meta.get(k) for k in ("status", "recheck_status", "dropped_spans", "wall_s")}),
          flush=True)
    if meta["status"] != "complete":
        raise SystemExit(1)


def add_commands(sub, source):
    parser = sub.add_parser("rollout-recheck",
                            help="recheck a pipeline's excluded candidates with the initial review's own reads")
    parser.add_argument("pipeline", help="a completed `pipeline` output (its fixed-window recheck is arm A)")
    parser.add_argument("out")
    parser.add_argument("--source", default=str(source))
    parser.add_argument("--config", default="~/.kloop/config.toml")
    parser.set_defaults(handler=command)
