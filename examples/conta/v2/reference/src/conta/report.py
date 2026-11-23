"""Summaries for the accountant."""

from __future__ import annotations

from conta.audit import audit
from conta.cui import check_cui
from conta.model import Invoice
from conta.vat import vat_breakdown


def summarize(invoices: list[Invoice]) -> dict:
    by_supplier: dict[str, float] = {}
    by_rate: dict[float, float] = {}
    for inv in invoices:
        key = check_cui(inv.supplier.cui).normalized
        by_supplier[key] = by_supplier.get(key, 0.0) + inv.gross_total
        for rate, g in vat_breakdown(inv).items():
            by_rate[rate] = by_rate.get(rate, 0.0) + g.net
    return {
        "count": len(invoices),
        "net": round(sum(i.net_total for i in invoices), 2),
        "vat": round(sum(i.vat_total for i in invoices), 2),
        "gross": round(sum(i.gross_total for i in invoices), 2),
        "by_supplier": {k: round(v, 2) for k, v in by_supplier.items()},
        "by_rate": {k: round(v, 2) for k, v in by_rate.items()},
        "needs_review": sum(audit(i).needs_review for i in invoices),
    }
