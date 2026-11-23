"""Confidence audit of extracted documents (basic version)."""

from __future__ import annotations

from conta.model import Invoice

REQUIRED = ("number", "issue_date", "supplier", "customer")


def completeness(inv: Invoice) -> float:
    """Fraction of required header fields that are present."""
    present = 0
    for f in REQUIRED:
        v = getattr(inv, f)
        if f in ("supplier", "customer"):
            present += bool(v.name and v.cui)
        else:
            present += bool(v)
    return present / len(REQUIRED)
