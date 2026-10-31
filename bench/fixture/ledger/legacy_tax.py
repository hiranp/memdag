VAT_RATES = {"DE": 19, "FR": 20, "NL": 21}


def compute(net_cents: int, country: str) -> int:
    return net_cents * VAT_RATES.get(country, 0) // 100
