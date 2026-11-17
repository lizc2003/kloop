#!/usr/bin/env python3
"""List blocking IO inside `async fn` bodies (plan 265).

The convention: filesystem and git helpers stay synchronous, and async callers
reach them through `spawn_blocking` (or a thin wrapper), never by calling
`std::fs::*` / `std::process::*` directly inside an async fn.  One blocking
stat or child process on a runtime worker stalls every other future on that
thread, including approvals and `turn/interrupt`.

This is a manual review aid, not a pass/fail gate: it is deliberately not part
of `make check`.  A hit is a question for the reviewer, not a proof; calls
inside `spawn_blocking` and test items are skipped, and a deliberate exception
can carry `// async-io-ok: <why>` on its line or just above it.

Usage: scripts/check-async-io.py [root ...]   (default: rust/crates)
"""

import pathlib
import re
import sys


def blank_non_code(source: str) -> str:
    """Replace comments and literals with spaces, keeping offsets and newlines.

    Brace counting needs a text where the only braces are code.  This is not a
    Rust parser: it handles line/block comments, strings, raw strings, and
    char literals closely enough that braces in those do not shift a body.
    """
    out = list(source)
    i = 0
    length = len(source)

    def blank(start: int, end: int) -> None:
        for index in range(start, min(end, length)):
            if out[index] != "\n":
                out[index] = " "

    while i < length:
        two = source[i : i + 2]
        if two == "//":
            end = source.find("\n", i)
            end = length if end < 0 else end
            blank(i, end)
            i = end
        elif two == "/*":
            depth = 1
            end = i + 2
            while end < length and depth:
                if source[end : end + 2] == "/*":
                    depth += 1
                    end += 2
                elif source[end : end + 2] == "*/":
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank(i, end)
            i = end
        elif source[i] == "r" or (two in ("br", "rb")):
            # Raw string: r"..." / r#"..."# / br#"..."#.
            start = i + (2 if source[i] == "b" else 1)
            hashes = 0
            while start < length and source[start] == "#":
                hashes += 1
                start += 1
            if start < length and source[start] == '"':
                terminator = '"' + "#" * hashes
                end = source.find(terminator, start + 1)
                end = length if end < 0 else end + len(terminator)
                blank(i, end)
                i = end
            else:
                i += 1
        elif source[i] == '"':
            end = i + 1
            while end < length:
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            blank(i, end)
            i = end
        elif source[i] == "'":
            # A char literal ('{', '\n', '\'') is a quote, a small escape or
            # character, then a quote.  Lifetimes (`'a`) have no closing quote
            # within that window and fall through.
            match = re.match(r"'(?:\\.|[^\\'])'", source[i:])
            if match:
                blank(i, i + match.end())
                i += match.end()
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def blank_test_items(blanked: str) -> str:
    """Blank every `#[cfg(... test ...)]` item, so tests do not drown the scan.

    Test code may touch the filesystem freely: it runs in a harness, not on a
    runtime worker other tasks share.
    """
    out = list(blanked)
    attributes = r"#\s*\[\s*(?:cfg\s*\([^\]]*\btest\b[^\]]*\)|(?:tokio::)?test\b[^\]]*)\]"
    for match in re.finditer(attributes, blanked):
        start = match.end()
        brace = blanked.find("{", start)
        semicolon = blanked.find(";", start)
        if brace < 0 or (semicolon >= 0 and semicolon < brace):
            end = semicolon + 1 if semicolon >= 0 else len(blanked)
        else:
            depth = 0
            end = brace
            while end < len(blanked):
                if blanked[end] == "{":
                    depth += 1
                elif blanked[end] == "}":
                    depth -= 1
                    if depth == 0:
                        end += 1
                        break
                end += 1
        for index in range(match.start(), end):
            if out[index] != "\n":
                out[index] = " "
    return "".join(out)


def blank_spawn_blocking(blanked: str) -> str:
    """Blank `spawn_blocking` closures: they are the sanctioned escape hatch."""
    out = list(blanked)
    for match in re.finditer(r"\bspawn_blocking\s*\(", blanked):
        brace = blanked.find("{", match.end())
        if brace < 0:
            continue
        depth = 0
        end = brace
        while end < len(blanked):
            if blanked[end] == "{":
                depth += 1
            elif blanked[end] == "}":
                depth -= 1
                if depth == 0:
                    end += 1
                    break
            end += 1
        for index in range(brace, end):
            if out[index] != "\n":
                out[index] = " "
    return "".join(out)


def async_fn_bodies(source: str, blanked: str):
    """Yield (header_line, body_start, body_end) offsets for every async fn."""
    for match in re.finditer(r"\basync\s+fn\b", blanked):
        body_start = blanked.find("{", match.end())
        if body_start < 0:
            continue
        depth = 0
        end = body_start
        while end < len(blanked):
            char = blanked[end]
            if char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    end += 1
                    break
            end += 1
        yield match.start(), body_start, end


def scan(path: pathlib.Path):
    source = path.read_text(encoding="utf-8", errors="replace")
    blanked = blank_spawn_blocking(blank_test_items(blank_non_code(source)))
    lines = source.splitlines()
    allowed = {
        index + 1
        for index, text in enumerate(lines)
        if "async-io-ok" in text
    }
    hits = []
    for header, body_start, body_end in async_fn_bodies(source, blanked):
        header_line = source.count("\n", 0, header) + 1
        body = blanked[body_start:body_end]
        for match in re.finditer(r"\bstd::fs::|\bstd::process::Command\b", body):
            offset = body_start + match.start()
            line = source.count("\n", 0, offset) + 1
            if any(near in allowed for near in (line - 3, line - 2, line - 1, line)):
                continue
            text = lines[line - 1].strip()
            hits.append((line, header_line, text))
    return hits


def main(argv):
    roots = argv[1:] or ["rust/crates"]
    total = 0
    for root in roots:
        for path in sorted(pathlib.Path(root).rglob("*.rs")):
            if (
                "tests" in path.parts
                or path.name in ("tests.rs", "testutil.rs")
                or path.stem.endswith("_tests")
            ):
                continue
            for line, header_line, text in scan(path):
                total += 1
                print(f"{path}:{line}: {text}")
                print(f"    inside the async fn starting at line {header_line}")
    if total:
        print(
            f"\n{total} blocking call(s) inside async fn bodies: "
            "route each through spawn_blocking or explain it (plan 265).",
            file=sys.stderr,
        )
        return 1
    print("no blocking std::fs / std::process calls inside async fn bodies")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))