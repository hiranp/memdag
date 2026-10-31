from dataclasses import dataclass

from ledger import errors


@dataclass
class Order:
    id: str
    total_cents: int
    currency: str
    age_days: int = 0


def create_order(order_id: str, total_cents: int, currency: str) -> Order:
    return Order(order_id, total_cents, currency)


def refund(order: Order) -> int:
    if order.age_days > 90:
        raise errors.RefundTooOld(order.id)
    return order.total_cents
