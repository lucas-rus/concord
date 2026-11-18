#!/usr/bin/env bash
# Creates a small but realistic Python project ("shop") as a git repository for
# live multi-agent experiments with `concord run`. See README.md next to it.
# Usage: setup.sh <target-dir>
set -euo pipefail
dir="${1:?usage: setup.sh <target-dir>}"
mkdir -p "$dir" && cd "$dir"
git init -q -b main
git config user.email "demo@example.com"
git config user.name "demo"
mkdir -p shop tests
: > shop/__init__.py
: > tests/__init__.py
printf '.concord/\n__pycache__/\n' > .gitignore

cat > shop/catalog.py <<'PY'
from dataclasses import dataclass
from decimal import Decimal


@dataclass(frozen=True)
class Product:
    sku: str
    name: str
    price: Decimal  # unit price in EUR


CATALOG = {
    "BOOK-001": Product("BOOK-001", "Domain-Driven Design", Decimal("49.90")),
    "BOOK-002": Product("BOOK-002", "Structure and Interpretation of Computer Programs", Decimal("39.50")),
    "MUG-001": Product("MUG-001", "Coffee mug", Decimal("12.00")),
}


def find_product(sku):
    try:
        return CATALOG[sku]
    except KeyError:
        raise KeyError(f"unknown product {sku}") from None
PY

cat > shop/pricing.py <<'PY'
from decimal import Decimal

from shop.catalog import find_product

VAT_RATE = Decimal("0.19")


def apply_discount(amount, percent):
    """Reduce `amount` by `percent` (0-100)."""
    if not 0 <= percent <= 100:
        raise ValueError("percent must be between 0 and 100")
    return amount * (Decimal(100) - Decimal(percent)) / Decimal(100)


def line_total(line):
    """Price of one order line (unit price times quantity), in EUR."""
    product = find_product(line.sku)
    return product.price * line.quantity


def subtotal(order):
    """Sum of all line totals, in EUR."""
    return sum((line_total(line) for line in order.lines), Decimal("0"))


def price_order(order):
    """Total to pay for an order in EUR: discounted subtotal plus VAT, rounded to cents."""
    discounted = apply_discount(subtotal(order), order.discount_percent)
    total = discounted * (1 + VAT_RATE)
    return total.quantize(Decimal("0.01"))
PY

cat > shop/orders.py <<'PY'
from dataclasses import dataclass, field

from shop.catalog import find_product
from shop.pricing import price_order


@dataclass
class OrderLine:
    sku: str
    quantity: int


@dataclass
class Order:
    customer: str
    lines: list = field(default_factory=list)
    discount_percent: int = 0


def create_order(customer, items, discount_percent=0):
    """Create an order from (sku, quantity) pairs."""
    order = Order(customer=customer, discount_percent=discount_percent)
    for sku, quantity in items:
        find_product(sku)
        if quantity <= 0:
            raise ValueError("quantity must be positive")
        order.lines.append(OrderLine(sku, quantity))
    return order


def checkout(order):
    """Summary handed to the payment provider."""
    total = price_order(order)
    return {"customer": order.customer, "items": len(order.lines), "total": str(total)}
PY

cat > shop/notifications.py <<'PY'
from shop.pricing import price_order


def receipt_text(order):
    """Plain-text receipt e-mailed to the customer after checkout."""
    total = price_order(order)
    lines = [
        f"Thank you for your order, {order.customer}!",
        f"Items: {len(order.lines)}",
        f"Total: {total} EUR",
    ]
    return "\n".join(lines)
PY

cat > tests/test_pricing.py <<'PY'
import unittest
from decimal import Decimal

from shop.orders import create_order
from shop.pricing import apply_discount, price_order, subtotal


class PricingTest(unittest.TestCase):
    def test_subtotal(self):
        order = create_order("ana", [("BOOK-001", 2), ("MUG-001", 1)])
        self.assertEqual(subtotal(order), Decimal("111.80"))

    def test_price_includes_vat(self):
        order = create_order("ana", [("MUG-001", 1)])
        self.assertEqual(price_order(order), Decimal("14.28"))

    def test_discount(self):
        self.assertEqual(apply_discount(Decimal("100"), 10), Decimal("90"))

    def test_discount_bounds(self):
        with self.assertRaises(ValueError):
            apply_discount(Decimal("10"), 120)


if __name__ == "__main__":
    unittest.main()
PY

cat > tests/test_orders.py <<'PY'
import unittest

from shop.orders import checkout, create_order


class OrdersTest(unittest.TestCase):
    def test_rejects_unknown_product(self):
        with self.assertRaises(KeyError):
            create_order("bob", [("NOPE", 1)])

    def test_rejects_non_positive_quantity(self):
        with self.assertRaises(ValueError):
            create_order("bob", [("MUG-001", 0)])

    def test_checkout_summary(self):
        order = create_order("bob", [("MUG-001", 1)])
        self.assertEqual(checkout(order), {"customer": "bob", "items": 1, "total": "14.28"})


if __name__ == "__main__":
    unittest.main()
PY

cat > tests/test_notifications.py <<'PY'
import unittest

from shop.notifications import receipt_text
from shop.orders import create_order


class NotificationsTest(unittest.TestCase):
    def test_receipt_mentions_total(self):
        order = create_order("carol", [("MUG-001", 1)])
        self.assertIn("Total: 14.28 EUR", receipt_text(order))


if __name__ == "__main__":
    unittest.main()
PY

git add -A
git commit -q -m "shop: catalog, pricing, orders, notifications"
python3 -m unittest discover -s tests -t . > /dev/null 2>&1 && echo "shop created in $(pwd); base tests pass"
