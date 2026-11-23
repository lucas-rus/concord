"""Romanian VAT rates and VAT breakdowns."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date

from conta.model import Invoice

CHANGE = date(2025, 8, 1)


def valid_rates(on: date) -> frozenset[float]:
    return frozenset({0.0, 5.0, 9.0, 19.0}) if on < CHANGE else frozenset({0.0, 11.0, 21.0})


@dataclass(frozen=True)
class VatGroup:
    net: float
    vat: float


def vat_breakdown(inv: Invoice) -> dict[float, VatGroup]:
    nets: dict[float, float] = {}
    for l in inv.lines:
        nets[l.vat_rate] = nets.get(l.vat_rate, 0.0) + l.total
    return {r: VatGroup(round(n, 2), round(n * r / 100, 2)) for r, n in nets.items()}
