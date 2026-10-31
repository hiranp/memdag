from ledger.orders import Order


def invoice_total(order: Order, country: str) -> int:
    """Total the customer pays for an order."""
    return order.total_cents
