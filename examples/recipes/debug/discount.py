"""Wrong charged price: subtracts the percent number instead of taking that percent off."""
import json
import sys


def apply_discount(price, percent):
    charged = price - percent
    return charged


order = json.load(sys.stdin)
print(json.dumps({"charged": apply_discount(order["price"], order["percent"])}))
