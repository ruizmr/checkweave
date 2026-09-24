"""Line totals in integer cents."""


def invoice_cents(lines):
    total = 0
    for line in lines:
        qty = line["qty"]
        price = line["cents"]
        if qty != 0 and price > 0:
            total += qty * price
    return total
