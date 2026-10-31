"""Memories a project like `fixture/ledger` would have accumulated: eight house rules that
the code doesn't state, plus unrelated distractors so search has to discriminate.

Each rule is phrased the way a past session would have recorded it, not the way a task
asks for the change, so a keyword search has to bridge different words.
"""

RULES = [
    {
        "id": "INV-money-cents",
        "kind": "invariant",
        "title": "Money is always integer cents",
        "body": "All amounts in ledger are int cents. Never use float or Decimal for money: "
        "floats caused the 2025 refund rounding incident (refunds off by a cent). Round with "
        "integer arithmetic.",
        "tags": "money cents rounding",
    },
    {
        "id": "INV-clock-utc",
        "kind": "invariant",
        "title": "Timestamps come from ledger.clock.now_utc()",
        "body": "Never call datetime.now() or datetime.utcnow() directly: naive local times "
        "broke the EU monthly reports. Use ledger.clock.now_utc() so tests can freeze time.",
        "tags": "time timezone",
    },
    {
        "id": "DEC-http-retry",
        "kind": "decision",
        "title": "Outbound HTTP goes through ledger.http.request_with_retry",
        "body": "ledger.http.get has no timeout and hung the workers during the provider "
        "outage. All outbound calls use request_with_retry (3 tries, 5 s timeout). Don't use "
        "requests or urllib directly.",
        "tags": "http network",
    },
    {
        "id": "DEC-config-get",
        "kind": "decision",
        "title": "Settings come from ledger.config.get()",
        "body": "Library code never reads os.environ or os.getenv; the deployment loader fills "
        "ledger.config at startup. Read settings with ledger.config.get(key, default).",
        "tags": "config settings",
    },
    {
        "id": "INV-payment-error",
        "kind": "invariant",
        "title": "Business-rule failures raise PaymentError",
        "body": "Raise ledger.errors.PaymentError or a subclass for validation and business-rule "
        "failures. The API layer only catches PaymentError; ValueError and bare Exception "
        "become HTTP 500s.",
        "tags": "errors exceptions",
    },
    {
        "id": "DEC-tax-v2",
        "kind": "decision",
        "title": "legacy_tax.compute is deprecated; use tax.compute_v2",
        "body": "ledger.legacy_tax.compute ignores reverse-charge VAT for B2B customers and was "
        "wrong in the 2026 audit. New code calls ledger.tax.compute_v2(net_cents, country, "
        "reverse_charge=...). legacy_tax stays only for old reports.",
        "tags": "tax vat",
    },
    {
        "id": "INV-no-print",
        "kind": "invariant",
        "title": "No print(); log with ledger.log.get_logger(__name__)",
        "body": "print() output is lost in production because stdout isn't collected. Use "
        "log = get_logger(__name__) and log.debug / log.info.",
        "tags": "logging",
    },
    {
        "id": "DEC-refund-90-days",
        "kind": "decision",
        "title": "Refunds are limited to 90 days because of chargebacks",
        "body": "After 90 days the card networks route disputes through the chargeback "
        "process, so refund() rejects older orders with RefundTooOld and support opens a "
        "chargeback instead.",
        "tags": "refunds",
    },
]

DISTRACTORS = [
    ("decision", "Admin UI uses React 19 with server components", "Chosen for streaming; see the frontend RFC."),
    ("decision", "Nightly reconciliation takes a Postgres advisory lock", "Prevents two schedulers from running it twice."),
    ("learning", "CSV exports are gzipped", "Exports shrink about 10x; the download endpoint sets Content-Encoding."),
    ("decision", "Feature flags are reviewed monthly", "Stale flags are removed in the first week of each month."),
    ("task", "Migrate CI runners to Ubuntu 26", "ubuntu-latest moves to 26 in October."),
    ("blocker", "Staging SMTP relay rejects TLS 1.3", "Email tests are skipped on staging until the relay is upgraded."),
    ("learning", "Mobile app caches the product catalogue for 24 h", "Price changes reach phones a day late."),
    ("decision", "Use semantic-release for version tags", "Commit messages follow Conventional Commits."),
    ("invariant", "Customer emails are never logged", "Logs are shipped to a third party; emails are PII."),
    ("decision", "Search uses OpenSearch, not Elasticsearch", "Licence change in 2021."),
    ("learning", "The fraud model is retrained weekly", "Retraining runs on Sunday nights."),
    ("decision", "Docker images are built with distroless bases", "Smaller attack surface."),
]
