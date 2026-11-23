"""BNR exchange rates and conversion to RON."""

from __future__ import annotations

from dataclasses import replace
from datetime import date

from conta.model import Invoice


class RateTable:
    def __init__(self, rates: dict[str, dict[date, float]] | None = None):
        self.rates = rates or {}

    @classmethod
    def from_csv(cls, text: str) -> "RateTable":
        rates: dict[str, dict[date, float]] = {}
        for line in text.splitlines():
            if not line.strip():
                continue
            d, cur, r = (x.strip() for x in line.split(","))
            rates.setdefault(cur.upper(), {})[date.fromisoformat(d)] = float(r)
        return cls(rates)

    def rate(self, currency: str, on: date) -> float:
        currency = currency.upper()
        if currency == "RON":
            return 1.0
        known = [d for d in self.rates.get(currency, {}) if d <= on]
        if not known:
            raise KeyError(f"no {currency} rate on or before {on}")
        return self.rates[currency][max(known)]


def to_ron(inv: Invoice, table: RateTable) -> Invoice:
    r = table.rate(inv.currency, inv.issue_date)

    def c(x: float) -> float:
        return round(x * r, 2)

    lines = [replace(l, unit_price=c(l.unit_price), total=c(l.total), vat_amount=c(l.vat_amount)) for l in inv.lines]
    return replace(
        inv,
        lines=lines,
        net_total=c(inv.net_total),
        vat_total=c(inv.vat_total),
        gross_total=c(inv.gross_total),
        currency="RON",
    )
