"""Domain model for Romanian fiscal documents."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date


@dataclass
class Party:
    """A supplier or customer as printed on the document."""

    name: str
    cui: str
    address: str = ""


@dataclass
class LineItem:
    description: str
    quantity: float
    unit_price: float
    vat_rate: float  # percent, e.g. 19.0
    total: float  # net line total as printed on the document


@dataclass
class Invoice:
    number: str
    issue_date: date
    supplier: Party
    customer: Party
    lines: list[LineItem] = field(default_factory=list)
    net_total: float = 0.0
    vat_total: float = 0.0
    gross_total: float = 0.0
    currency: str = "RON"
