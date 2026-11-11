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
GREP_LINE = re.compile(r"^(?:(?P<rev>[0-9a-fA-F]{7,40}|HEAD[\^~0-9]*):)?(?P<path>[^:\s][^:\n]*):(?P<line>\d+):")
NUMBERED = re.compile(r"^(\d+)\t")
# bash 内联截断说明（tools/bash.rs `format_output`）与会话落盘预览的结尾（history.rs `POINTER_END`）。
BASH_TRUNCATION = "\n[output truncated: "
OFFLOAD_POINTER_END = "so only what you extract enters the context]"
NL_PREFIX = re.compile(r"^\s*\d+\t")
# 只认几种写法完全固定的读取命令（可带一个 `cd 目录 &&` 前缀、末尾一个读标准输入的
# `| head -n N`）。整条匹配，所以带文件参数的过滤器、输入重定向、多文件、额外的命令段都
# 匹配不上，输出来源唯一；其余写法一律不计。
_CD = r"""(?:cd\s+(?P<cd>[^\s;&|<>]+)\s*(?:&&|;)\s*)?"""
_FILE = r"""(?P<path>'[^']+'|"[^"]+"|[^\s;&|<>'"]+)"""
_RANGE = r"""sed\s+-n\s+(?P<q>['"]?)(?P<a>\d+)(?:,(?P<b>\d+))?p(?P=q)"""
_REV = r"""git\s+show\s+(?P<rev>[^\s:;&|<>'"]+):(?P<path>[^\s;&|<>'"]+)"""
_HEAD = r"""(?:\s*\|\s*head\s+(?:-n\s*)?-?\d+)?"""
READ_SHAPES = [
    ("sed", re.compile(rf"^{_CD}{_RANGE}\s+{_FILE}{_HEAD}$")),
    ("nl", re.compile(rf"^{_CD}(?P<nl>nl\s+-ba)\s+{_FILE}\s*\|\s*{_RANGE}{_HEAD}$")),
    ("show-blob", re.compile(rf"^{_CD}{_REV}\s*\|\s*(?:(?P<nl>nl\s+-ba)\s*\|\s*)?{_RANGE}{_HEAD}$")),
    ("cat", re.compile(rf"^{_CD}cat\s+{_FILE}{_HEAD}$")),
    ("show-blob", re.compile(rf"^{_CD}{_REV}{_HEAD}$")),
]
READ_VERB = re.compile(r"\b(sed\s+-n|cat|nl|head|tail|git\s+show)\b")
GREP_ONE_FILE = re.compile(rf"""^{_CD}(?:grep|rg)\s+(?:-\S*n\S*\s+)(?:-\S+\s+)*(?:'[^']*'|"[^"]*"|\S+)\s+{_FILE}$""")


def exact_read(command):
    """命令完整匹配某个固定读取形状时返回各字段，否则 None。"""
    text = command.strip()
    for kind, shape in READ_SHAPES:
        if match := shape.match(text):
            return {**match.groupdict(), "kind": kind}
    return None


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
        if cd := re.match(_CD, command.strip()).group("cd"):
            target = unquote(cd)
            base = os.path.realpath(target if os.path.isabs(target) else os.path.join(base, target))
        if read := exact_read(command):
            self.credit_read(read, shown, base)
        elif READ_VERB.search(command):
            # 读取写法不在固定形状里：输出可能另有来源（多段、文件参数、重定向），不计。
            self.counts["unrecognized-skipped"] = self.counts.get("unrecognized-skipped", 0) + 1
        single = None
        if match := GREP_ONE_FILE.match(command.strip()):
            path = self.relative(match.group("path"), base)
            single = path if path and self.file_lines("after", path) else None
        self.patch(shown)
        self.grep_output(shown, base, single)

    def credit_read(self, read, shown, base):
        """固定形状的单个读取：在输出里找对应冻结行从第一行起的最长连续前缀，只计这一段
        （截断与 `| head` 由此排除）。"""
        rev = read.get("rev")
        version = self.version_of(rev) if rev else "after"
        path = unquote(read["path"]) if rev else self.relative(read["path"], base)
        content = self.file_lines(version, path) if version and path else None
        start = int(read["a"]) if read.get("a") else 1
        if not content or start > len(content):
            return
        # 反向范围按 sed 语义只打印起始行。
        end = len(content) if not read.get("a") else max(start, int(read["b"] or start))
        expected = content[start - 1:min(end, len(content))]
        lines = shown.splitlines()
        if read.get("nl"):
            lines = [NL_PREFIX.sub("", line, count=1) for line in lines]
        length = longest_shown_prefix(expected, lines)
        if length:
            self.add(version, path, range(start, start + length), read["kind"])
        if length < len(expected):
            self.counts["unshown"] = self.counts.get("unshown", 0) + len(expected) - length

    def read_file(self, arguments, output):
        path = self.relative(arguments.get("path", ""), str(self.snapshot))
        numbers = [int(m.group(1)) for line in output.splitlines() if (m := NUMBERED.match(line))]
        self.add("after", path, numbers, "read_file")

    def grep_tool(self, arguments, output):
        target = self.relative(arguments.get("path", "."), str(self.snapshot))
        single = target if target and self.file_lines("after", target) else None
        self.grep_output(output, str(self.snapshot), single)


def longest_shown_prefix(expected, lines):
    """`expected` 从第一行起在 `lines` 里连续出现的最长长度；空的 `expected` 为 0。"""
    best = 0
    for index, line in enumerate(lines):
        if not expected or line != expected[0]:
            continue
        length = 0
        while (length < len(expected) and index + length < len(lines)
               and lines[index + length] == expected[length]):
            length += 1
        best = max(best, length)
        if best == len(expected):
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
                try:
                    if call.get("name") == "bash":
                        reads.bash(arguments.get("command", ""), output)
                    elif call.get("name") == "read_file":
                        reads.read_file(arguments, output)
                    elif call.get("name") == "grep":
                        reads.grep_tool(arguments, output)
                except (ValueError, IndexError, KeyError, TypeError, AttributeError):
                    # 读法超出解析器的假设时这一条不计（保守），不能让整次复核准备中断。
                    reads.counts["unparsed"] = reads.counts.get("unparsed", 0) + 1
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
