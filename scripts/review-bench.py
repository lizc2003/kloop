#!/usr/bin/env python3
"""Plan 272 的审查完备度基准：生成带预埋缺陷的仓库、跑一组对照、统计结果。

    scripts/review-bench.py make <dir>
    scripts/review-bench.py run <out-dir> --skill <SKILL.md> [--system-prompt <file>]
                                [--kloop <binary>] [--max-rounds 80]
                                [--commit <sha> [--source <repo>]]
    scripts/review-bench.py stats <out-dir>...
    scripts/review-bench.py probe-reasoning [--provider <name>] [--trials 3]
    scripts/review-bench.py recheck-prepare <cases.json> <packet-dir> [--source <repo>]
    scripts/review-bench.py recheck <packet-dir> <out-dir> [--provider <name>] [--trials 3]
                                   [--max-output-tokens 8192]
    scripts/review-bench.py recheck-stats <out-dir>...
    scripts/review-bench.py pipeline <out-dir> --commit <sha> [--kloop <binary>] [--evidence-tools]
    scripts/review-bench.py pipeline-recheck <initial-out> <out-dir>
    scripts/review-bench.py evidence-recheck <packet-dir> <out-dir> --sampling <protocol.json>
                                           [--with-tools]

`make` 生成一个 Python 小项目：基础版本提交一次，审查对象是工作区里未提交的改动
（含一个未跟踪的新文件）。缺陷 D1–D8 与诱饵见 `ANSWER_KEY`，判分规则见 plan 272。

`run --commit <sha>` 改为审查一个真实提交：仓库只含父版本快照和这个提交两次提交，
后来的修复提交不在对象库里，审查目标是 `HEAD`。

`run` 每次都新建仓库和临时 HOME：配置从 `~/.kloop/config.toml` 复制（0600），运行
结束就删掉副本，rollout 留在 `<out-dir>/home` 供 `stats` 读。提示是 skill 正文把
`$ARGUMENTS` 换成空串，和 TUI 里不带参数的 `/code-review` 得到的文本一致；同一份
skill 还放进仓库的 `.kloop/skills/code-review/` 覆盖内置版本，模型另外调 skill
工具时拿到的也是这一组的版本。kloop 的 macOS 沙箱不能嵌套，必须在外层执行沙箱
之外运行。

`probe-reasoning` 直接请求 Responses 轨：在历史里放一个合成推理项（形状与 kloop 回放
无签名推理相同），其中记一个随机暗号，再分别带上 / 不带上它问模型暗号是什么。只有
带上时能答对，才说明网关把回传的推理交给了模型。
"""
import argparse
import json
import os
import pathlib
import random
import re
import shutil
import subprocess
import sys
import time
import tomllib
import urllib.request

import review_recheck
import review_pipeline
import review_evidence

ANSWER_KEY = {
    "D1": "report.paginate: end = start + size - 1 drops the last item of every page",
    "D2": "money.parse_amount now returns cents; billing.line_total_cents still multiplies by 100",
    "D3": "invoice.discount_code is written by to_dict but never read by from_dict",
    "D4": "config.load_config swallows every error and returns {}, disabling the currency allowlist",
    "D5": "untracked export.py joins a request-supplied filename: path traversal / arbitrary write",
    "D6": "sync.push_with_retry never counts RateLimited: unbounded retry loop",
    "D7": "report.newest_first lost reverse=True: oldest first, docstring still says newest",
    "D8": "report.unique_customers uses a list for membership: O(n^2) over the full history",
    "decoy": "report.page_count (len + size - 1) // size is a correct ceiling division",
}

# ---- 基准仓库内容 -------------------------------------------------------------
# 基础版本（提交）与改动后版本（工作区）。改动只出现在 CHANGED 列出的文件里；
# billing.py 不改，它是 D2 那个没跟上的调用方。

MONEY_BASE = '''"""Parsing and formatting of money amounts."""
from decimal import Decimal, InvalidOperation


class AmountError(ValueError):
    pass


def parse_amount(text: str) -> Decimal:
    """Parse a user-entered amount such as "1,234.50" into dollars."""
    cleaned = text.strip().replace(",", "").lstrip("$")
    if not cleaned:
        raise AmountError("empty amount")
    try:
        value = Decimal(cleaned)
    except InvalidOperation as exc:
        raise AmountError(f"not a number: {text!r}") from exc
    if value < 0:
        raise AmountError("negative amount")
    return value.quantize(Decimal("0.01"))


def format_amount(dollars: Decimal) -> str:
    return f"${dollars:,.2f}"
'''

MONEY_CHANGED = '''"""Parsing and formatting of money amounts."""
from decimal import Decimal, InvalidOperation


class AmountError(ValueError):
    pass


def parse_amount(text: str) -> int:
    """Parse a user-entered amount such as "1,234.50" into integer cents.

    Integer cents keep sums exact; format them with format_cents().
    """
    cleaned = text.strip().replace(",", "").lstrip("$")
    if not cleaned:
        raise AmountError("empty amount")
    try:
        value = Decimal(cleaned)
    except InvalidOperation as exc:
        raise AmountError(f"not a number: {text!r}") from exc
    if value < 0:
        raise AmountError("negative amount")
    return int(value.quantize(Decimal("0.01")) * 100)


def format_amount(dollars: Decimal) -> str:
    return f"${dollars:,.2f}"


def format_cents(cents: int) -> str:
    return f"${cents / 100:,.2f}"
'''

BILLING = '''"""Invoice totals, in integer cents."""
from .money import parse_amount


def line_total_cents(line: dict) -> int:
    """Total of one invoice line in cents: unit price times quantity."""
    unit = parse_amount(line["unit_price"])
    return int(unit * 100) * int(line["quantity"])


def invoice_total_cents(lines: list[dict]) -> int:
    return sum(line_total_cents(line) for line in lines)


def tax_cents(total_cents: int, rate_percent: int) -> int:
    """Tax rounded half up to the cent."""
    return (total_cents * rate_percent + 50) // 100


def grand_total_cents(lines: list[dict], rate_percent: int) -> int:
    subtotal = invoice_total_cents(lines)
    return subtotal + tax_cents(subtotal, rate_percent)
'''

CONFIG_BASE = '''"""Service configuration."""
import json
import os

DEFAULT_PATH = os.path.expanduser("~/.invoicer.json")


def load_config(path: str | None = None) -> dict:
    """Load the JSON config; a missing file means an empty config."""
    path = path or os.environ.get("INVOICER_CONFIG", DEFAULT_PATH)
    if not os.path.exists(path):
        return {}
    with open(path) as fh:
        return json.load(fh)


def is_currency_allowed(cfg: dict, currency: str) -> bool:
    """Whether invoices may be issued in `currency`."""
    allowed = cfg.get("allowed_currencies")
    if not allowed:
        # No allowlist configured: every currency is accepted.
        return True
    return currency.upper() in {code.upper() for code in allowed}


def export_enabled(cfg: dict) -> bool:
    return bool(cfg.get("export", {}).get("enabled", False))
'''

CONFIG_CHANGED = '''"""Service configuration."""
import json
import os

DEFAULT_PATH = os.path.expanduser("~/.invoicer.json")


def load_config(path: str | None = None) -> dict:
    """Load the JSON config.

    A broken or unreadable config file must never stop the service from
    starting, so any problem reading it yields an empty config.
    """
    path = path or os.environ.get("INVOICER_CONFIG", DEFAULT_PATH)
    try:
        with open(path) as fh:
            return json.load(fh)
    except Exception:
        return {}


def is_currency_allowed(cfg: dict, currency: str) -> bool:
    """Whether invoices may be issued in `currency`."""
    allowed = cfg.get("allowed_currencies")
    if not allowed:
        # No allowlist configured: every currency is accepted.
        return True
    return currency.upper() in {code.upper() for code in allowed}


def export_enabled(cfg: dict) -> bool:
    return bool(cfg.get("export", {}).get("enabled", False))
'''

INVOICE_BASE = '''"""The invoice model and its persistence."""
from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import date, timedelta

STATUSES = ("draft", "sent", "paid", "void")


@dataclass
class Line:
    description: str
    quantity: int
    unit_price: str

    def to_dict(self) -> dict:
        return {
            "description": self.description,
            "quantity": self.quantity,
            "unit_price": self.unit_price,
        }

    @classmethod
    def from_dict(cls, data: dict) -> "Line":
        return cls(data["description"], int(data["quantity"]), data["unit_price"])


@dataclass
class Invoice:
    id: str
    customer: str
    due_date: date
    lines: list[Line] = field(default_factory=list)
    status: str = "draft"
    notes: str = ""

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "customer": self.customer,
            "due_date": self.due_date.isoformat(),
            "lines": [line.to_dict() for line in self.lines],
            "status": self.status,
            "notes": self.notes,
        }

    # -- lifecycle ---------------------------------------------------------

    def send(self) -> None:
        if self.status != "draft":
            raise ValueError(f"cannot send a {self.status} invoice")
        if not self.lines:
            raise ValueError("cannot send an empty invoice")
        self.status = "sent"

    def mark_paid(self) -> None:
        if self.status != "sent":
            raise ValueError(f"cannot mark a {self.status} invoice paid")
        self.status = "paid"

    def void(self, reason: str) -> None:
        if self.status == "paid":
            raise ValueError("a paid invoice cannot be voided")
        self.status = "void"
        self.note(f"voided: {reason}")

    def reopen(self) -> None:
        if self.status != "void":
            raise ValueError("only a void invoice can be reopened")
        self.status = "draft"

    # -- lines -------------------------------------------------------------

    def add_line(self, description: str, quantity: int, unit_price: str) -> None:
        self._require_draft()
        self.lines.append(Line(description, quantity, unit_price))

    def remove_line(self, index: int) -> Line:
        self._require_draft()
        return self.lines.pop(index)

    def replace_line(self, index: int, line: Line) -> None:
        self._require_draft()
        self.lines[index] = line

    def line_count(self) -> int:
        return len(self.lines)

    def _require_draft(self) -> None:
        if self.status != "draft":
            raise ValueError(f"lines of a {self.status} invoice are frozen")

    # -- dates -------------------------------------------------------------

    def is_overdue(self, today: date) -> bool:
        return self.status == "sent" and today > self.due_date

    def days_overdue(self, today: date) -> int:
        if not self.is_overdue(today):
            return 0
        return (today - self.due_date).days

    def extend_due_date(self, days: int) -> None:
        if days <= 0:
            raise ValueError("an extension must be at least one day")
        self.due_date = self.due_date + timedelta(days=days)

    # -- validation --------------------------------------------------------

    def validate(self) -> list[str]:
        problems = []
        if not self.id:
            problems.append("missing id")
        if not self.customer.strip():
            problems.append("missing customer")
        if self.status not in STATUSES:
            problems.append(f"unknown status {self.status!r}")
        for index, line in enumerate(self.lines):
            if line.quantity <= 0:
                problems.append(f"line {index}: quantity must be positive")
            if not line.description.strip():
                problems.append(f"line {index}: missing description")
        return problems

    # -- presentation ------------------------------------------------------

    def summary(self) -> str:
        return f"{self.id} {self.customer} due {self.due_date} [{self.status}]"

    def note(self, text: str) -> None:
        self.notes = f"{self.notes}\\n{text}".strip()

    def short_customer(self, width: int = 24) -> str:
        if len(self.customer) <= width:
            return self.customer
        return self.customer[: width - 1] + "…"

    def label(self) -> str:
        return f"Invoice {self.id}"

    def is_editable(self) -> bool:
        return self.status == "draft"

    def is_closed(self) -> bool:
        return self.status in ("paid", "void")

    def reminder_text(self, today: date) -> str:
        days = self.days_overdue(today)
        if days == 0:
            return ""
        return f"{self.label()} is {days} day(s) overdue."

    def copy_as_draft(self, new_id: str) -> "Invoice":
        return Invoice(
            id=new_id,
            customer=self.customer,
            due_date=self.due_date,
            lines=[Line(l.description, l.quantity, l.unit_price) for l in self.lines],
        )

    def matches(self, query: str) -> bool:
        query = query.lower()
        return query in self.id.lower() or query in self.customer.lower()

    def sort_tuple(self) -> tuple:
        return (self.due_date, self.id)

    @classmethod
    def from_dict(cls, data: dict) -> "Invoice":
        return cls(
            id=data["id"],
            customer=data["customer"],
            due_date=date.fromisoformat(data["due_date"]),
            lines=[Line.from_dict(item) for item in data.get("lines", [])],
            status=data.get("status", "draft"),
            notes=data.get("notes", ""),
        )


def save(invoices: list[Invoice], path: str) -> None:
    with open(path, "w") as fh:
        json.dump([inv.to_dict() for inv in invoices], fh, indent=2)


def load(path: str) -> list[Invoice]:
    with open(path) as fh:
        return [Invoice.from_dict(item) for item in json.load(fh)]
'''

# D3：新字段进了 dataclass、to_dict 和 validate，from_dict 没跟上。
INVOICE_EDITS = [
    (
        'from dataclasses import dataclass, field\n',
        'import re\nfrom dataclasses import dataclass, field\n',
    ),
    (
        'STATUSES = ("draft", "sent", "paid", "void")\n',
        'STATUSES = ("draft", "sent", "paid", "void")\n'
        'DISCOUNT_CODE = re.compile(r"^[A-Z0-9]{4,12}$")\n',
    ),
    (
        '    notes: str = ""\n\n    def to_dict',
        '    notes: str = ""\n    discount_code: str | None = None\n\n    def to_dict',
    ),
    (
        '            "notes": self.notes,\n        }\n\n    # -- lifecycle',
        '            "notes": self.notes,\n            "discount_code": self.discount_code,\n'
        '        }\n\n    def apply_discount(self, code: str) -> None:\n'
        '        self._require_draft()\n        self.discount_code = code.strip().upper()\n\n'
        '    # -- lifecycle',
    ),
    (
        '        if self.status not in STATUSES:\n'
        '            problems.append(f"unknown status {self.status!r}")\n',
        '        if self.status not in STATUSES:\n'
        '            problems.append(f"unknown status {self.status!r}")\n'
        '        if self.discount_code is not None and not DISCOUNT_CODE.match(self.discount_code):\n'
        '            problems.append(f"malformed discount code {self.discount_code!r}")\n',
    ),
]

REPORT_BASE = '''"""Reports over invoices.

Reports run over the full invoice history: the yearly export walks every
invoice ever issued, a few hundred thousand rows for the larger tenants.
"""
from collections import defaultdict


def newest_first(invoices):
    """Invoices sorted by due date, newest first; ties broken by id."""
    return sorted(invoices, key=lambda inv: (inv.due_date, inv.id), reverse=True)


def unique_customers(invoices):
    """Customer names in first-seen order."""
    seen = set()
    out = []
    for inv in invoices:
        if inv.customer not in seen:
            seen.add(inv.customer)
            out.append(inv.customer)
    return out


def paginate(items, page, size):
    """Return page `page` (1-based) of `size` items."""
    if page < 1 or size < 1:
        raise ValueError("bad page")
    return items[(page - 1) * size : page * size]


def totals_by_status(invoices, total_of):
    totals = defaultdict(int)
    for inv in invoices:
        totals[inv.status] += total_of(inv)
    return dict(totals)
'''

REPORT_CHANGED = '''"""Reports over invoices.

Reports run over the full invoice history: the yearly export walks every
invoice ever issued, a few hundred thousand rows for the larger tenants.
"""
from collections import defaultdict


def _sort_key(inv):
    return (inv.due_date, inv.id)


def newest_first(invoices):
    """Invoices sorted by due date, newest first; ties broken by id."""
    return sorted(invoices, key=_sort_key)


def by_customer(invoices):
    """Invoices grouped by customer, each group in due-date order."""
    groups = defaultdict(list)
    for inv in sorted(invoices, key=_sort_key):
        groups[inv.customer].append(inv)
    return dict(groups)


def unique_customers(invoices):
    """Customer names in first-seen order."""
    seen = []
    for inv in invoices:
        if inv.customer not in seen:
            seen.append(inv.customer)
    return seen


def paginate(items, page, size):
    """Return page `page` (1-based) of `size` items."""
    if page < 1 or size < 1:
        raise ValueError("page and size start at 1")
    start = (page - 1) * size
    end = start + size - 1
    return items[start:end]


def page_count(items, size):
    """Number of pages needed to show every item."""
    return (len(items) + size - 1) // size


def totals_by_status(invoices, total_of):
    totals = defaultdict(int)
    for inv in invoices:
        totals[inv.status] += total_of(inv)
    return dict(totals)
'''

API_BASE = '''"""HTTP handlers. `request.args` is the parsed query string."""
from .config import is_currency_allowed, load_config
from .report import newest_first, paginate


def handle_list(request, invoices):
    page = int(request.args.get("page", 1))
    return {"items": [inv.to_dict() for inv in paginate(newest_first(invoices), page, 50)]}


def handle_create(request, store):
    cfg = load_config()
    currency = request.args["currency"]
    if not is_currency_allowed(cfg, currency):
        return {"error": f"currency {currency} is not allowed"}, 400
    inv = store.create(customer=request.args["customer"], currency=currency)
    return {"id": inv.id}, 201
'''

API_CHANGED = '''"""HTTP handlers. `request.args` is the parsed query string."""
from .config import export_enabled, is_currency_allowed, load_config
from .export import export_invoice
from .report import newest_first, page_count, paginate


def handle_list(request, invoices):
    page = int(request.args.get("page", 1))
    ordered = newest_first(invoices)
    return {
        "items": [inv.to_dict() for inv in paginate(ordered, page, 50)],
        "pages": page_count(ordered, 50),
    }


def handle_create(request, store):
    cfg = load_config()
    currency = request.args["currency"]
    if not is_currency_allowed(cfg, currency):
        return {"error": f"currency {currency} is not allowed"}, 400
    inv = store.create(customer=request.args["customer"], currency=currency)
    return {"id": inv.id}, 201


def handle_export(request, store):
    if not export_enabled(load_config()):
        return {"error": "export is disabled"}, 403
    inv = store.get(request.args["id"])
    name = request.args.get("name", f"{inv.id}.json")
    return {"path": export_invoice(inv, name)}, 200
'''

# D5：未跟踪的新文件。
EXPORT_NEW = '''"""Write invoices to the shared export directory for the accounting team."""
import json
import os

EXPORT_DIR = "/srv/invoicer/exports"


def export_invoice(inv, filename: str) -> str:
    """Write `inv` as JSON under EXPORT_DIR and return the path written."""
    path = os.path.join(EXPORT_DIR, filename)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        json.dump(inv.to_dict(), fh, indent=2)
    return path
'''

SYNC_BASE = '''"""Push invoices to the accounting service."""
import time


class TransientError(Exception):
    """A failure worth retrying (timeouts, 5xx)."""


class SyncFailed(Exception):
    pass


def push_with_retry(push, batch, max_attempts=5, backoff=0.5, sleep=time.sleep):
    """Push one batch, retrying transient failures at most `max_attempts` times."""
    for attempt in range(1, max_attempts + 1):
        try:
            return push(batch)
        except TransientError:
            if attempt == max_attempts:
                raise SyncFailed(f"gave up after {max_attempts} attempts")
            sleep(backoff * 2 ** attempt)


def batches(items, size=100):
    for start in range(0, len(items), size):
        yield items[start : start + size]
'''

# D6：限流分支 continue 前不计数。
SYNC_CHANGED = '''"""Push invoices to the accounting service."""
import time


class TransientError(Exception):
    """A failure worth retrying (timeouts, 5xx)."""


class RateLimited(Exception):
    """HTTP 429; `retry_after` is the server's Retry-After in seconds."""

    def __init__(self, retry_after: float):
        super().__init__(f"rate limited, retry after {retry_after}s")
        self.retry_after = retry_after


class SyncFailed(Exception):
    pass


def push_with_retry(push, batch, max_attempts=5, backoff=0.5, sleep=time.sleep):
    """Push one batch, retrying transient failures at most `max_attempts` times."""
    attempt = 0
    while attempt < max_attempts:
        try:
            return push(batch)
        except RateLimited as exc:
            # The server said exactly how long to wait; honor it and try again.
            sleep(exc.retry_after)
            continue
        except TransientError:
            attempt += 1
            sleep(backoff * 2 ** attempt)
    raise SyncFailed(f"gave up after {max_attempts} attempts")


def batches(items, size=100):
    for start in range(0, len(items), size):
        yield items[start : start + size]
'''

README = """# invoicer

Invoice tooling: the invoice model, totals, reports, HTTP handlers and the
sync to the accounting service. Python 3.11+.
"""

CURRENCIES = ["USD", "EUR", "GBP", "JPY", "CNY", "CHF", "CAD", "AUD", "SEK", "NOK"]


def rates_json(seed: int) -> str:
    """约 540 行的汇率表；两个 seed 每一行都不同，diff 约 40K 字符（考 G3）。"""
    rows = {}
    value = 0.8 + seed * 0.013
    for day in range(1, 7):
        for base in CURRENCIES:
            for quote in CURRENCIES:
                if base == quote:
                    continue
                value = (value * 1.000371 + 0.0007 * seed) % 3 + 0.1
                rows[f"{base}-{quote}-2026-10-{day:02d}"] = round(value, 6)
    return json.dumps(rows, indent=1) + "\n"


# ---- make ------------------------------------------------------------------

def write(root: pathlib.Path, rel: str, text: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def git(root: pathlib.Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-c", "user.name=bench", "-c", "user.email=bench@example.com", *args],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout


def changed_invoice() -> str:
    text = INVOICE_BASE
    for old, new in INVOICE_EDITS:
        if text.count(old) != 1:
            sys.exit(f"invoice edit anchor is not unique: {old[:40]!r}")
        text = text.replace(old, new)
    return text


def make(root: pathlib.Path) -> None:
    if root.exists() and any(root.iterdir()):
        sys.exit(f"{root} is not empty")
    root.mkdir(parents=True, exist_ok=True)
    git(root, "init", "-q", "-b", "main")
    base = {
        "README.md": README,
        "invoicer/__init__.py": '"""Invoice tooling."""\n',
        "invoicer/api.py": API_BASE,
        "invoicer/billing.py": BILLING,
        "invoicer/config.py": CONFIG_BASE,
        "invoicer/invoice.py": INVOICE_BASE,
        "invoicer/money.py": MONEY_BASE,
        "invoicer/report.py": REPORT_BASE,
        "invoicer/schemas/rates.json": rates_json(1),
        "invoicer/sync.py": SYNC_BASE,
    }
    for rel, text in base.items():
        write(root, rel, text)
    git(root, "add", "-A")
    git(root, "commit", "-qm", "Initial import")
    changed = {
        "invoicer/api.py": API_CHANGED,
        "invoicer/config.py": CONFIG_CHANGED,
        "invoicer/invoice.py": changed_invoice(),
        "invoicer/money.py": MONEY_CHANGED,
        "invoicer/report.py": REPORT_CHANGED,
        "invoicer/schemas/rates.json": rates_json(2),
        "invoicer/sync.py": SYNC_CHANGED,
        "invoicer/export.py": EXPORT_NEW,
    }
    for rel, text in changed.items():
        write(root, rel, text)


# ---- run -------------------------------------------------------------------

def skill_body(text: str, arguments: str) -> str:
    """去掉 frontmatter，`$ARGUMENTS` 换成审查目标（空串 = 审查工作区）。"""
    if text.startswith("---\n"):
        end = text.index("\n---\n", 4)
        text = text[end + 5 :]
    return text.lstrip("\n").replace("$ARGUMENTS", arguments)


def snapshot(source: pathlib.Path, sha: str, root: pathlib.Path) -> None:
    """只含两次提交的审查仓库：父版本快照，加上 `sha` 本身（保留原提交信息）。

    不克隆历史，是为了让后来的修复提交（标准答案）不在对象库里。
    """
    root.mkdir(parents=True)
    git(root, "init", "-q", "-b", "main")

    def extract(rev: str) -> None:
        archive = subprocess.run(["git", "-C", str(source), "archive", "--format=tar", rev],
                                 check=True, capture_output=True).stdout
        subprocess.run(["tar", "-x", "-C", str(root)], input=archive, check=True)

    extract(f"{sha}^")
    git(root, "add", "-A")
    git(root, "commit", "-qm", "Snapshot of the parent revision")
    git(root, "rm", "-rq", ".")
    extract(sha)
    git(root, "add", "-A")
    message = subprocess.run(["git", "-C", str(source), "log", "-1", "--format=%B", sha],
                             check=True, capture_output=True, text=True).stdout
    git(root, "commit", "-qm", message)


def drop_top_level_key(text: str, key: str) -> str:
    """去掉第一个表头之前的 `key = …`；值是多行字符串时连同后续行整段去掉。

    结果再用 tomllib 核对：只少了这一个键、其余不变，否则直接报错，不悄悄写坏配置。
    """
    head = re.compile(rf"""\s*["']?{re.escape(key)}["']?\s*=\s*""")
    lines = text.splitlines(keepends=True)
    out, index, top = [], 0, True
    while index < len(lines):
        line = lines[index]
        index += 1
        if re.match(r"\s*\[", line):
            top = False
        match = head.match(line) if top else None
        if not match:
            out.append(line)
            continue
        value = line[match.end():]
        delimiter = value[:3] if value[:3] in ('"""', "'''") else None
        if delimiter:
            body = value[3:]
            while not closes_multiline(body, delimiter) and index < len(lines):
                body += lines[index]
                index += 1
    result = "".join(out)
    expected = tomllib.loads(text)
    expected.pop(key, None)
    if tomllib.loads(result) != expected:
        sys.exit(f"could not remove {key} from the config cleanly; edit it by hand")
    return result


def closes_multiline(body: str, delimiter: str) -> bool:
    """多行字符串在 body 里是否已闭合。基本字符串里奇数个反斜杠后的引号是转义；字面
    字符串没有转义。"""
    if delimiter == "'''":
        return delimiter in body
    start = 0
    while (found := body.find(delimiter, start)) != -1:
        backslashes = len(body[:found]) - len(body[:found].rstrip("\\"))
        if backslashes % 2 == 0:
            return True
        start = found + 1
    return False


def prepare_config(real_state: pathlib.Path, state: pathlib.Path, system_prompt: str | None,
                   config_path: pathlib.Path | None = None) -> str:
    """临时 HOME 用的配置正文，以及它引用的系统提示文件。

    显式 `--system-prompt` 替换原有的 `system_prompt_file`；没给时沿用原配置，相对路径
    的提示文件跟着复制，否则临时 HOME 下启动就会因找不到文件失败。
    """
    text = (config_path or real_state / "config.toml").read_text()
    current = tomllib.loads(text).get("system_prompt_file")
    if system_prompt:
        shutil.copyfile(system_prompt, state / "system-prompt.md")
        text = 'system_prompt_file = "system-prompt.md"\n' + drop_top_level_key(
            text, "system_prompt_file"
        )
    elif current and not pathlib.Path(current).is_absolute():
        target = state / current
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(real_state / current, target)
    tomllib.loads(text)
    return text


def run(args) -> None:
    out = pathlib.Path(args.out).resolve()
    if out.exists():
        sys.exit(f"{out} already exists")
    repo, home = out / "repo", out / "home"
    if args.commit:
        snapshot(pathlib.Path(args.source).resolve(), args.commit, repo)
    else:
        make(repo)
    skill = pathlib.Path(args.skill).read_text()
    write(repo, ".kloop/skills/code-review/SKILL.md", skill)
    with open(repo / ".git/info/exclude", "a") as fh:
        fh.write(".kloop/\n")
    state = home / ".kloop"
    state.mkdir(parents=True)
    os.chmod(state, 0o700)
    source_config = pathlib.Path(getattr(args, "config", "~/.kloop/config.toml")).expanduser().resolve()
    config = prepare_config(source_config.parent, state, args.system_prompt, source_config)
    config_path = state / "config.toml"
    config_path.touch(mode=0o600)
    config_path.write_text(config)
    prompt = skill_body(skill, "HEAD" if args.commit else "")
    (out / "prompt.md").write_text(prompt)
    env = dict(os.environ, HOME=str(home))
    # 临时 HOME 下没有工具链；指回本机的 cargo/rustup，模型跑 cargo 时与平时一致。
    for var, name in (("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")):
        real = pathlib.Path.home() / name
        if var not in env and real.is_dir():
            env[var] = str(real)
    cmd = [args.kloop, "--headless", "--json", "--max-rounds", str(args.max_rounds), prompt]
    started = time.time()
    try:
        with open(out / "events.ndjson", "w") as stdout, open(out / "stderr.txt", "w") as stderr:
            code = subprocess.run(cmd, cwd=repo, env=env, stdout=stdout, stderr=stderr,
                                  timeout=args.timeout).returncode
    except subprocess.TimeoutExpired:
        code = "timeout"
    finally:
        config_path.unlink()
    meta = {"exit": code, "wall_s": round(time.time() - started, 1), "skill": args.skill,
            "system_prompt": args.system_prompt, "commit": args.commit}
    (out / "meta.json").write_text(json.dumps(meta, indent=2))
    print(json.dumps(meta))


# ---- stats -----------------------------------------------------------------

# 模型是否“看见过”某个缺陷所在的代码：工具结果里出现过这段文本就算看见。
# 用来区分“没看到”和“看到了没报”。
EXPOSURE = {
    "D3_from_dict": 'notes=data.get("notes", ""),',
    "D5_export": "os.path.join(EXPORT_DIR, filename)",
    "D6_sync": "except RateLimited as exc:",
    "D2_caller": "int(unit * 100)",
}


def text_of(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text_of(item.get("text", item.get("content", ""))) for item in content
                         if isinstance(item, dict))
    return ""


def stats_one(out: pathlib.Path) -> dict:
    rollouts = sorted((out / "home/.kloop/projects").glob("*/*/sessions/*.jsonl"))
    if not rollouts:
        return {"run": out.name, "error": "no rollout"}
    rounds, tools, bash = 0, {}, []
    usage = {"input": 0, "cache_read": 0, "output": 0, "requests": 0}
    truncated_bash = offloaded = 0
    exposure = {key: False for key in EXPOSURE}
    final = ""
    for line in rollouts[-1].read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        if kind == "provider_usage":
            u = event.get("usage") or {}
            usage["input"] += u.get("input_tokens", 0)
            usage["cache_read"] += u.get("cache_read_input_tokens", 0)
            usage["output"] += u.get("output_tokens", 0)
            usage["requests"] += 1
        if kind != "message":
            continue
        if event.get("role") == "assistant":
            rounds += 1
            texts = []
            for block in event.get("content", []):
                if block.get("type") == "tool_use":
                    name = block.get("name", "?")
                    tools[name] = tools.get(name, 0) + 1
                    if name == "bash":
                        bash.append(block.get("input", {}).get("command", "")[:160])
                elif block.get("type") == "text":
                    texts.append(block.get("text", ""))
            if texts:
                final = "\n".join(texts)
        else:
            for block in event.get("content", []):
                if block.get("type") != "tool_result":
                    continue
                body = text_of(block.get("content"))
                if re.search(r"\[output truncated: \d+ characters", body):
                    truncated_bash += 1
                if "full output saved to" in body:
                    offloaded += 1
                for key, needle in EXPOSURE.items():
                    if needle in body:
                        exposure[key] = True
    (out / "report.md").write_text(final)
    meta = json.loads((out / "meta.json").read_text()) if (out / "meta.json").exists() else {}
    return {
        "run": out.name, "exit": meta.get("exit"), "wall_min": round(meta.get("wall_s", 0) / 60, 1),
        "rounds": rounds, "tool_calls": sum(tools.values()), "tools": tools,
        "usage": usage, "bash_truncated": truncated_bash, "offloaded": offloaded,
        "saw": exposure, "bash": bash, "report_chars": len(final),
    }


def stats(args) -> None:
    for out in args.outs:
        print(json.dumps(stats_one(pathlib.Path(out)), ensure_ascii=False))


# ---- probe-reasoning -------------------------------------------------------

def responses_call(provider: dict, input_items: list, effort: str, tools=None) -> dict:
    """流式请求一次，返回 `response.completed` 里的 response 对象。"""
    body = {
        "model": provider["model"], "input": input_items, "store": False,
        "include": ["reasoning.encrypted_content"], "stream": True,
        "reasoning": {"effort": effort, "summary": "auto"}, "max_output_tokens": 4096,
    }
    if tools:
        body["tools"] = tools
    headers = {"Content-Type": "application/json", **provider.get("auth_header", {})}
    request = urllib.request.Request(provider["base_url"].rstrip("/") + "/responses",
                                     data=json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(request, timeout=300) as response:
        for raw in response:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if payload == "[DONE]":
                break
            event = json.loads(payload)
            if event.get("type") == "response.completed":
                return event["response"]
    raise RuntimeError("stream ended without response.completed")


def answer_text(response: dict) -> str:
    return "".join(
        part.get("text", "")
        for item in response.get("output", []) if item.get("type") == "message"
        for part in item.get("content", [])
    )


# 推理项必须落在“当前这一轮”（最后一条用户消息之后、工具调用之前）：DeepSeek 的
# 对话模板本来就丢弃更早轮次的推理，放在上一轮测不出回传是否生效。
TASK = ("Call the lookup tool once with q set to 'status'. After it returns, reply with only "
        "the code word your reasoning noted before the call. If you cannot see that "
        "reasoning, reply: unknown")
LOOKUP = [{"type": "function", "name": "lookup", "description": "Look up a status value.",
           "parameters": {"type": "object", "properties": {"q": {"type": "string"}},
                          "required": ["q"]}}]
SHAPES = {
    # kloop 回放无签名推理的形状：文本在 summary，encrypted_content 为空。
    "summary": lambda note: {"type": "reasoning", "encrypted_content": None,
                             "summary": [{"type": "summary_text", "text": note}]},
    # Responses 规范里推理原文的位置。
    "content": lambda note: {"type": "reasoning", "encrypted_content": None, "summary": [],
                             "content": [{"type": "reasoning_text", "text": note}]},
}


def probe_reasoning(args) -> None:
    config = tomllib.loads((pathlib.Path.home() / ".kloop/config.toml").read_text())
    name = args.provider or config["provider"]
    provider = config["providers"][name]
    if provider.get("wire_api") != "responses":
        sys.exit(f"{name} is not a responses provider")
    if args.model:
        provider = dict(provider, model=args.model)
    print(f"provider={name} model={provider['model']} effort={args.effort}")
    for trial in range(1, args.trials + 1):
        animal = random.choice(["PELICAN", "MARBLE", "CEDAR", "QUARTZ", "FALCON"])
        word = f"{animal}-{random.randint(1000, 9999)}"
        note = f"I need to call lookup first. Code word to remember: {word}."
        call = [
            {"type": "function_call", "call_id": "call_probe_1", "name": "lookup",
             "arguments": json.dumps({"q": "status"})},
            {"type": "function_call_output", "call_id": "call_probe_1", "output": "done"},
        ]
        row = {"trial": trial, "code_word": word}
        for shape, build in [*SHAPES.items(), ("none", None)]:
            reasoning = [build(note)] if build else []
            items = [{"role": "user", "content": TASK}, *reasoning, *call]
            reply = answer_text(responses_call(provider, items, args.effort, LOOKUP)).strip()
            row[shape] = reply[:40]
        print(json.dumps(row, ensure_ascii=False))


# ---- main ------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("make")
    p.add_argument("dir")
    p = sub.add_parser("run")
    p.add_argument("out")
    p.add_argument("--skill", required=True)
    p.add_argument("--system-prompt")
    p.add_argument("--config", default="~/.kloop/config.toml")
    p.add_argument("--kloop", default="kloop")
    p.add_argument("--max-rounds", type=int, default=80)
    p.add_argument("--timeout", type=int, default=3600)
    p.add_argument("--commit", help="review this commit of --source instead of the synthetic repo")
    p.add_argument("--source", default=str(pathlib.Path(__file__).resolve().parent.parent))
    p = sub.add_parser("stats")
    p.add_argument("outs", nargs="+")
    p = sub.add_parser("probe-reasoning")
    p.add_argument("--provider")
    p.add_argument("--model")
    p.add_argument("--effort", default="high")
    p.add_argument("--trials", type=int, default=3)
    review_recheck.add_commands(sub, pathlib.Path(__file__).resolve().parent.parent)
    review_pipeline.add_commands(sub, pathlib.Path(__file__).resolve().parent.parent,
                                 sys.modules[__name__])
    review_evidence.add_commands(sub, pathlib.Path(__file__).resolve().parent.parent)
    args = parser.parse_args()
    if hasattr(args, "handler"):
        args.handler(args)
    elif args.command == "make":
        make(pathlib.Path(args.dir).resolve())
    elif args.command == "run":
        run(args)
    elif args.command == "stats":
        stats(args)
    else:
        probe_reasoning(args)


if __name__ == "__main__":
    main()
