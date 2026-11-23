"""Fiscal receipts (bon fiscal)."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date

from conta.model import LineItem, Party


@dataclass
class Receipt:
    number: str
    issue_date: date
    supplier: Party
    lines: list[LineItem] = field(default_factory=list)
    total: float = 0.0
    payment: str = "numerar"
