#!/usr/bin/env python3
"""Plan 128 的两个复查数字，从一份 kloop rollout 里算出来。

    scripts/review-pace.py [rollout.jsonl] [任务关键字]

不给路径就取最近改动的那份 rollout。两个数字分别对应 plan 128 的两处改动：

  [2] 爬到 200k 用了几轮  → SKILL.md「大改动分批取 diff」是否生效（越大越好）
  [1] 压缩后的工具/轮      → 摘要第 9/11 节是否生效（回到 3 以上才算）

[1] **必须配着单工具轮的成分读**：压缩后的塌（成分以探索为主，仍在重建已知内容）
和任务收尾时的塌（成分以跑测试为主）数值上无法区分，只有成分能区分。没压缩的
`e1f59ef4` 后半也是 1.3，那是收尾，拿它当失败证据就错了。
"""
import collections
import json
import pathlib
import sys

CONTEXT_MARK = 200_000
ROLLOUTS = pathlib.Path.home() / ".kloop/projects/v1"


def latest_rollout() -> pathlib.Path:
    files = sorted(ROLLOUTS.glob("*/sessions/*.jsonl"), key=lambda p: p.stat().st_mtime)
    if not files:
        sys.exit(f"no rollout under {ROLLOUTS}")
    return files[-1]


def kind(call: dict) -> str:
    """一次工具调用属于探索还是收尾 —— [1] 的判读全靠这个分类。"""
    name = call["name"]
    if name in ("read_file", "grep", "glob"):
        return "探索"
    if name == "bash":
        command = call.get("input", {}).get("command", "")
        return "收尾" if "test" in command else "其他"
    return "其他"


def tasks_in(path: pathlib.Path, want: str | None):
    current, out = None, []
    for line in path.open():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind_ = event.get("type")
        if kind_ == "message" and event.get("role") == "user":
            text = " ".join(
                c.get("text", "") for c in event.get("content", []) if c.get("type") == "text"
            ).strip()
            if text.startswith("审查") or (want and want in text):
                current = {
                    "name": text[:60], "ctx": [], "calls": [],
                    "compacted": [], "rounds": 0, "t0": None, "t1": None,
                    "terminal": None,
                }
                out.append(current)
            continue
        if current is None:
            continue
        if ts := event.get("ts"):
            current["t0"] = current["t0"] or ts
            current["t1"] = ts
        if kind_ == "provider_usage":
            usage = event["usage"]
            current["rounds"] += 1
            current["ctx"].append(
                usage["input_tokens"] + usage.get("cache_read_input_tokens", 0)
            )
        elif kind_ == "compacted":
            current["compacted"].append(current["rounds"])
        elif kind_ == "turn_terminal":
            # 没有这一条就说明任务还在跑；此时的墙钟和批处理度都是中途读数，
            # 拿它们当结论会把「还没收敛」误读成「收敛得很好」。
            current["terminal"] = (event.get("status"), current["t1"])
        elif kind_ == "message" and event.get("role") == "assistant":
            calls = [c for c in event.get("content", []) if c.get("type") == "tool_use"]
            if calls:
                current["calls"].append((bool(current["compacted"]), calls))
    return out


def report(task: dict) -> None:
    print(f"\n=== {task['name']} ===")
    if not task["terminal"]:
        elapsed = (task["t1"] - task["t0"]) / 60000 if task["t0"] else 0
        print(f"!! 未见终态：仍在跑或被中断（已 {elapsed:.1f} min, {task['rounds']} 轮）")
        print("   下面的数字都是中途读数，不能当结论")
    elif task["t0"]:
        status, end = task["terminal"]
        print(f"墙钟 {(end - task['t0']) / 60000:.1f} min, {task['rounds']} 轮采样, 终态 {status}")

    reached = next(
        (i + 1 for i, ctx in enumerate(task["ctx"]) if ctx >= CONTEXT_MARK), None
    )
    print(f"[2] 爬到 200k: {reached or '未到'} 轮   (基线 kloop 10 / codex 31，越大越好)")
    if task["compacted"]:
        rounds = "、".join(f"第 {r} 轮" for r in task["compacted"])
        note = "（压缩不止一次说明它在反复撞窗口）" if len(task["compacted"]) > 1 else ""
        print(f"    压缩点: {rounds}{note}")
    else:
        print("    未压缩")

    post = [calls for after, calls in task["calls"] if after]
    if not post:
        print("[1] 未压缩，本指标不适用")
        return
    sizes = [len(calls) for calls in post]
    singles = [calls[0] for calls in post if len(calls) == 1]
    print(
        f"[1] 压缩后 {len(sizes)} 轮, 平均 {sum(sizes) / len(sizes):.1f} 工具/轮, "
        f"单工具轮 {100 * len(singles) / len(sizes):.0f}%"
        "   (基线 1.4-1.5 / 86%，回到 3 以上才算奏效)"
    )
    if singles:
        mix = collections.Counter(kind(c) for c in singles)
        print(f"    单工具轮成分: {dict(mix)}   ← 探索占多数=仍在重建；收尾为主=正常")


def main() -> None:
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    path = pathlib.Path(args[0]) if args and args[0].endswith(".jsonl") else latest_rollout()
    want = args[-1] if args and not args[-1].endswith(".jsonl") else None
    print(f"# {path}")
    found = [t for t in tasks_in(path, want) if not want or want in t["name"]]
    if not found:
        sys.exit("no review task found in that rollout")
    for task in found:
        report(task)


if __name__ == "__main__":
    main()
