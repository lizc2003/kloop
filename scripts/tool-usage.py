#!/usr/bin/env python3
"""一个工具在真实会话里实际被怎么用——从本机所有 rollout 里量出来。

    scripts/tool-usage.py read_file --since 20260903 --lines-vs limit
    scripts/tool-usage.py read_file --marker "call read_file with offset="
    scripts/tool-usage.py bash --key command

rollout 是 append-only 的 jsonl，一行一条 `message`，`tool_use` 和 `tool_result`
按 `id` / `tool_use_id` 配对。把这两半拼起来，就能回答"这个常量在真实使用里咬到
几次"，而不是按代码或按文件大小分布去猜。

**这个脚本存在的理由**（plan 154 收尾那次问答）：当时判断 `READ_CONTENT_CHARS = 30_000`
偏小，依据是"kloop 自己 37% 的 .rs 文件超过 30 000 字符"。**文件大小的分布不是行为的
分布**，而两者读起来很像。量完的结论是不该抬：抬到 30 000 之后被字符预算咬到的读取
只剩 1.5%，而 7 000 时代是 41.9%——收益已经被 plan 106 那次吃掉了。

同一次测量我连读错三次，三个坑都在这里，用之前先看一遍：

  **1. 按常量变更的日期把语料切开。**不切就是拿旧常量的会话在评新常量。read_file 的
  截断提示换过措辞（旧 `[read output truncated; …]`，新 `[showing lines A-B of C; …]`），
  靠它认出分界是 2026-09-03；没有这种措辞差别时用 git log 找那次提交的日期。`--since`。

  **2. "参数没给"和"参数给了但等于零"是两回事。**`limit: 0` 在 read_file 里是"按预算给
  全部"，和不给 limit 同义。只判 `"limit" not in input` 会漏掉一半整文件读——我第一遍
  报的 11 次，实际是 47 次。

  **3. 带续读提示 ≠ 撞上限，少拿到行也 ≠ 撞上限。**模型自己要一个 150 行的窗口、文件
  还有更多，同样会带续读提示；而"要了 200 行只拿到 150 行"也可能只是文件到头了。两个
  条件必须同时成立才算字符预算在咬：**拿到的行数少于要的，而且结果自己说还有更多**。
  `--lines-vs` 这一栏就是它，也是整件事唯一决定性的数字。少一个 and，同一份语料给出的
  是 32.4% 而不是 0.7%——我在这上面连栽两次。

其余输出怎么读：

  结果字符数分布   贴着某个常量的尖峰 = 那个常量在咬。plan 106 当年就是靠
                   "中位 6400、最大 7919、没有一个超过 8000"认出 7 000 的上限的。
  参数出现频率     模型实际给了哪些参数。一个"默认全量"的参数如果 95% 的调用都显式
                   给了，说明模型不走默认路径，针对默认值的优化没有意义。
  重复率           同一个目标被读/搜了几次。往返花在哪里，这一栏通常比上限那栏重要：
                   30 000 时代仍是 70%，而上限只咬 1.5%。
  区间重叠         `--overlap`：这次读的行区间与本会话已读区间相交吗？相交 ⟹ 那些行
                   还在上下文里，这次是冗余重读。**压缩边界会清空**，因为压缩把旧结果
                   换走了，此时重读正当——不清空，31.1% 里有一半是冤枉的（真值 13.8%）。
                   plan 151 的判定就是这一栏。

**必读的陷阱**：语料是本机 dogfood，几乎总是偏向最近那几次任务。脚本会打印项目桶
分布——一个桶占九成以上时，结论只对那一类工作负载成立，别当成通用结论。
"""

import argparse
import collections
import json
import pathlib
import re
import sys

ROLLOUTS = pathlib.Path.home() / ".kloop/projects/v1"
NUMBERED_LINE = re.compile(r"^\d+\t")


def calls(tool: str):
    """(项目桶, 会话日期, 入参, 结果文本, 是否 is_error)，按 rollout 配对。"""
    for path in sorted(ROLLOUTS.glob("*/sessions/*.jsonl")):
        bucket = path.parent.parent.name[:9]
        day = path.stem[:8]
        pending: dict[str, dict] = {}
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            try:
                item = json.loads(line)
            except ValueError:
                continue
            if item.get("type") != "message":
                continue
            for block in item.get("content") or []:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "tool_use" and block.get("name") == tool:
                    pending[block.get("id")] = block.get("input") or {}
                elif block.get("type") == "tool_result":
                    args = pending.pop(block.get("tool_use_id"), None)
                    if args is not None:
                        yield bucket, day, args, result_text(block), bool(block.get("is_error"))


def result_text(block: dict) -> str:
    content = block.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(b.get("text", "") for b in content if isinstance(b, dict))
    return ""


def percentiles(values: list[int]) -> str:
    values = sorted(values)
    n = len(values)
    pick = lambda q: values[min(n - 1, int(n * q))]  # noqa: E731
    return f"中位 {pick(0.5)}  p90 {pick(0.9)}  p99 {pick(0.99)}  max {values[-1]}"


def report_lines_vs(rows: list, key: str) -> None:
    """模型要了多少行，实际拿到几行。

    少拿到**且结果说还有更多**才是字符预算把窗口截短了；少拿到而没有续读提示，
    那是文件到头了，与预算无关。漏掉这个 and，32.4% 和 0.7% 的差别就出来了。
    """
    asked = [r for r in rows if positive_count(r[2].get(key))]
    short = [r for r in asked if count_lines(r[3]) < r[2][key] and has_more(r[3])]
    full = [r for r in rows if positive_count(r[2].get(key)) is None]
    print(f"\n字符预算咬到几次（--lines-vs {key}）:")
    if not any(count_lines(r[3]) for r in rows):
        print(f"  这个工具的结果不是 `{{n}}\\t` 编号行，这一栏对它没有意义")
        return
    print(
        f"  给了 {key} 却少拿到行: {len(short)} / {len(asked)} 次带 {key} 的调用"
        f"   = 全部调用的 {len(short) * 100 / len(rows):.1f}%"
    )
    if short:
        deficit = sorted(r[2][key] - count_lines(r[3]) for r in short)
        print(f"    少给的行数: {percentiles(deficit)}")
    print(f"  没给 {key}（=要全部）却被截断: {len(full_truncated(full))} / {len(full)} 次")


def full_truncated(rows: list) -> list:
    return [r for r in rows if has_more(r[3])]


def has_more(text: str) -> bool:
    """结果自己说"还没读完"——续读提示或行内截断提示。"""
    return "to continue]" in text or "truncated within line" in text


def report_overlap(tool: str, since: str | None) -> None:
    """read_file 专用：这次读的行区间，和本会话已经读过的区间相交吗？

    相交 ⟹ 那些行**还在模型的上下文里**，这次读是冗余的。三分之一的读取落在这
    一档，但其中约一半由压缩解释得通（压缩把旧结果换走了，重读是正当的），所以
    压缩边界必须清空区间集合——不清空会把正当行为算成打转。plan 151 的判定就是
    这一栏，它的验收也拿这个数复算。
    """
    fresh = paged = overlap = 0
    worst: collections.Counter = collections.Counter()
    for path in sorted(ROLLOUTS.glob("*/sessions/*.jsonl")):
        if since and path.stem[:8] <= since:
            continue
        seen: dict[str, list[tuple[int, int]]] = collections.defaultdict(list)
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            try:
                item = json.loads(line)
            except ValueError:
                continue
            if item.get("type") == "compacted":
                seen.clear()
                continue
            if item.get("type") != "message":
                continue
            for block in item.get("content") or []:
                if not (isinstance(block, dict) and block.get("type") == "tool_use"):
                    continue
                if block.get("name") != tool:
                    continue
                args = block.get("input") or {}
                target = args.get("path")
                start = positive_count(args.get("offset")) or 1
                span = positive_count(args.get("limit"))
                end = start + span if span else 1 << 30
                before = seen[target]
                if not before:
                    fresh += 1
                elif any(start < r[1] and r[0] < end for r in before):
                    overlap += 1
                    worst[target] += 1
                else:
                    paged += 1
                before.append((start, end))
    total = fresh + paged + overlap
    if not total:
        print(f"\n区间重叠: 语料里没有带 path 的 {tool} 调用")
        return
    print(f"\n区间重叠（压缩边界清零后）: {total} 次 {tool}")
    print(f"  首次读这个目标  : {fresh} ({fresh * 100 / total:.1f}%)")
    print(f"  读了新区间      : {paged} ({paged * 100 / total:.1f}%)")
    print(f"  **重读已读过的** : {overlap} ({overlap * 100 / total:.1f}%)")
    for target, count in worst.most_common(5):
        print(f"    {count:>4}  {target}")


def count_lines(text: str) -> int:
    return sum(1 for line in text.split("\n") if NUMBERED_LINE.match(line))


def positive_count(value: object) -> int | None:
    """正整数入参。bool 在 Python 里是 int 的子类，排掉，否则 `-n: true` 会被
    当成数值统计出一个"中位 True"。"""
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return value


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tool", help="工具名，例如 read_file / grep / bash")
    parser.add_argument("--since", help="只看这个日期之后的会话（YYYYMMDD，不含当天）")
    parser.add_argument("--marker", help="tool_result 里的固定文本，统计命中率")
    parser.add_argument("--key", help="按哪个入参算重复率（默认自动挑第一个字符串参数）")
    parser.add_argument("--lines-vs", help="拿这个数值入参和结果里的编号行数对比")
    parser.add_argument(
        "--overlap",
        action="store_true",
        help="按 offset/limit 算行区间，统计重读已读区间的比例（压缩边界清零）",
    )
    args = parser.parse_args()

    if not ROLLOUTS.is_dir():
        sys.exit(f"没有 rollout: {ROLLOUTS}")
    rows = [r for r in calls(args.tool) if not args.since or r[1] > args.since]
    if not rows:
        sys.exit(f"语料里没有 {args.tool} 的调用")

    days = [day for _, day, _, _, _ in rows]
    buckets = collections.Counter(bucket for bucket, _, _, _, _ in rows)
    print(f"{args.tool}: {len(rows)} 次调用，{min(days)} → {max(days)}")
    top_share = buckets.most_common(1)[0][1] * 100 / len(rows)
    print(f"项目桶: {dict(buckets.most_common())}" + ("   ⚠ 单一负载" if top_share > 90 else ""))

    ok = [r for r in rows if not r[4]]
    print(f"成功 {len(ok)}，报错 {len(rows) - len(ok)}")
    if not ok:
        return
    print(f"结果字符数: {percentiles([len(text) for _, _, _, text, _ in ok])}")

    keys = collections.Counter(k for _, _, a, _, _ in ok for k in a)
    print("\n参数出现频率:")
    for key, count in keys.most_common():
        line = f"  {key:<12} {count:>6} ({count * 100 / len(ok):.1f}%)"
        nums = sorted(
            n for _, _, a, _, _ in ok if (n := positive_count(a.get(key))) is not None
        )
        if nums:
            line += f"   正值: {percentiles(nums)}"
        print(line)

    key = args.key or next(
        (k for k in ("path", "pattern", "command", "notebook_path") if k in keys), None
    )
    if key:
        targets = collections.Counter(
            a[key] for _, _, a, _, _ in ok if isinstance(a.get(key), str)
        )
        total = sum(targets.values())
        if total:
            print(
                f"\n按 {key} 的重复率: {len(targets)} 个目标 / {total} 次 = "
                f"{(total - len(targets)) * 100 / total:.0f}%"
            )
            for target, count in targets.most_common(5):
                print(f"  {count:>4}  {target}")

    if args.lines_vs:
        report_lines_vs(ok, args.lines_vs)

    if args.overlap:
        report_overlap(args.tool, args.since)

    if args.marker:
        hit = [r for r in ok if args.marker in r[3]]
        print(f"\nmarker 命中: {len(hit)} / {len(ok)} = {len(hit) * 100 / len(ok):.2f}%")
        if hit:
            print(f"  命中那批的结果字符数: {percentiles([len(r[3]) for r in hit])}")


if __name__ == "__main__":
    main()
