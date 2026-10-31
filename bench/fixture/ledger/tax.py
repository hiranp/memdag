VAT_RATES = {"DE": 19, "FR": 20, "NL": 21}


def compute_v2(net_cents: int, country: str, reverse_charge: bool = False) -> int:
    if reverse_charge:
        return 0
    return net_cents * VAT_RATES.get(country, 0) // 100
