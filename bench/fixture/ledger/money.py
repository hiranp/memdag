def to_cents(amount: str) -> int:
    """Parse a decimal amount string such as "12.34" into cents."""
    whole, _, frac = amount.partition(".")
    return int(whole) * 100 + int((frac + "00")[:2])


def format_cents(cents: int) -> str:
    return f"{cents // 100}.{cents % 100:02d}"
