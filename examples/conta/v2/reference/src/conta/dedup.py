"""Duplicate invoice detection."""

from __future__ import annotations

from conta.cui import check_cui
from conta.model import Invoice


def find_duplicates(invoices: dict[str, Invoice]) -> list[list[str]]:
    groups: dict[tuple[str, str], list[str]] = {}
    for doc_id, inv in invoices.items():
        key = (check_cui(inv.supplier.cui).normalized, "".join(inv.number.split()).upper())
        groups.setdefault(key, []).append(doc_id)
    return sorted((sorted(g) for g in groups.values() if len(g) > 1), key=lambda g: g[0])
