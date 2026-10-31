class PaymentError(Exception):
    pass


class RefundTooOld(PaymentError):
    pass
