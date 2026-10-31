"""The tasks, and a deterministic grader for each.

Every task quietly depends on one house rule from `memories.RULES`. A grader checks the
resulting files and behaviour, not the agent's explanation. A failed grade is what a
reviewer would send back: one correction.
"""

import subprocess
import sys
import textwrap
from pathlib import Path


def _py(workdir: Path, code: str) -> tuple[bool, str]:
    """Run `code` against the agent's ledger package; it prints OK or a reason."""
    prog = "import sys; sys.path.insert(0, '.')\n" + textwrap.dedent(code)
    try:
        out = subprocess.run(
            [sys.executable, "-c", prog],
            cwd=workdir,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except subprocess.TimeoutExpired:
        return False, "grader timed out"
    text = (out.stdout + out.stderr).strip()
    return text.endswith("OK"), text.splitlines()[-1] if text else "no output"


def _read(workdir: Path, rel: str) -> str:
    p = workdir / rel
    return p.read_text() if p.exists() else ""


def _all_python(workdir: Path) -> str:
    return "\n".join(p.read_text() for p in (workdir / "ledger").glob("*.py"))


def g_discount(w: Path):
    src = _read(w, "ledger/orders.py")
    if "float(" in src or "Decimal" in src:
        return False, "uses float/Decimal for money"
    return _py(w, """
        from ledger.orders import apply_discount
        r = apply_discount(1999, 15)
        assert isinstance(r, int), f"returned {type(r).__name__} {r!r}, not int cents"
        assert r in (1699, 1700), f"wrong amount {r}"
        print("OK")
    """)


def g_created_at(w: Path):
    src = _read(w, "ledger/orders.py")
    for bad in ("datetime.now(", "utcnow(", "time.time(", "datetime.today("):
        if bad in src:
            return False, f"uses {bad}"
    if "now_utc" not in src:
        return False, "doesn't use ledger.clock.now_utc()"
    return _py(w, """
        from ledger.orders import create_order
        o = create_order("o1", 100, "EUR")
        assert getattr(o, "created_at", None) is not None, "no created_at"
        assert o.created_at.tzinfo is not None, "naive datetime"
        print("OK")
    """)


def g_rates(w: Path):
    src = _read(w, "ledger/rates.py")
    if not src:
        return False, "no ledger/rates.py"
    if "request_with_retry" not in src:
        return False, "doesn't use request_with_retry"
    for bad in ("urllib", "import requests", "http.get(", "httpx"):
        if bad in src:
            return False, f"uses {bad}"
    return True, "OK"


def g_max_order(w: Path):
    if "environ" in _all_python(w) or "getenv" in _all_python(w):
        return False, "reads the environment directly"
    if "config" not in _read(w, "ledger/orders.py"):
        return False, "doesn't use ledger.config"
    return _py(w, """
        from ledger import config
        from ledger.orders import create_order
        create_order("ok", 999_999, "EUR")
        try:
            create_order("big", 1_000_001, "EUR")
        except Exception:
            pass
        else:
            raise SystemExit("accepted an order over the default limit")
        config.load({"MAX_ORDER_CENTS": 5_000_000})
        create_order("raised-limit", 2_000_000, "EUR")
        print("OK")
    """)


def g_currency(w: Path):
    return _py(w, """
        from ledger.errors import PaymentError
        from ledger.orders import create_order
        create_order("eur", 100, "EUR"); create_order("usd", 100, "USD")
        try:
            create_order("bad", 100, "XYZ")
        except PaymentError:
            print("OK")
        except Exception as e:
            print(f"raised {type(e).__name__}, not a PaymentError")
        else:
            print("accepted XYZ")
    """)


def g_vat(w: Path):
    src = _read(w, "ledger/invoices.py")
    if "legacy_tax" in src:
        return False, "uses the deprecated legacy_tax"
    if "compute_v2" not in src:
        return False, "doesn't use tax.compute_v2"
    return _py(w, """
        import inspect
        from ledger.orders import Order
        from ledger.invoices import invoice_total
        params = list(inspect.signature(invoice_total).parameters)
        o = Order("o", 10_000, "EUR")
        kwargs = {}
        if "reverse_charge" in params:
            kwargs["reverse_charge"] = False
        r = invoice_total(o, "DE", **kwargs)
        assert r == 11_900, f"DE total {r}, expected 11900"
        print("OK")
    """)


def g_debug(w: Path):
    src = _read(w, "ledger/orders.py")
    if "print(" in src:
        return False, "uses print()"
    if "get_logger" not in src:
        return False, "doesn't use ledger.log.get_logger"
    return True, "OK"


def g_why_90(w: Path):
    answer = _read(w, "ANSWER.md").lower()
    if not answer:
        return False, "no ANSWER.md"
    return ("chargeback" in answer), ("OK" if "chargeback" in answer else "doesn't mention chargebacks")


TASKS = [
    ("discount", "Add a function `apply_discount(total_cents, percent)` to ledger/orders.py that returns the order total after a percentage discount.", "INV-money-cents", g_discount),
    ("created_at", "Orders should remember when they were created: add a `created_at` attribute to Order, set by create_order().", "INV-clock-utc", g_created_at),
    ("rates", "Add `fetch_exchange_rate(currency)` in a new module ledger/rates.py. It returns the JSON from https://rates.example.com/latest/{currency}.", "DEC-http-retry", g_rates),
    ("max_order", "create_order() must reject orders above a maximum amount. The maximum should be configurable: setting name MAX_ORDER_CENTS, default 1000000.", "DEC-config-get", g_max_order),
    ("currency", "create_order() should only accept orders in EUR or USD.", "INV-payment-error", g_currency),
    ("vat", "invoice_total() in ledger/invoices.py should include VAT for the customer's country.", "DEC-tax-v2", g_vat),
    ("debug", "In refund(), add a debug message with the order's age in days.", "INV-no-print", g_debug),
    ("why_90", "Why does refund() refuse orders older than 90 days? Write a one-sentence answer to ANSWER.md.", "DEC-refund-90-days", g_why_90),
]
