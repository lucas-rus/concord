"""Confidence audit of extracted documents."""

from __future__ import annotations

from dataclasses import dataclass, field

from conta.cui import check_cui
from conta.model import Invoice

REQUIRED = ("number", "issue_date", "supplier", "customer")
WEIGHTS = {"completeness": 0.20, "line_math": 0.25, "global_math": 0.20, "cui": 0.15, "plausibility": 0.10, "cross_reference": 0.10}
RATES = frozenset({0.0, 5.0, 9.0, 19.0})


@dataclass
class AuditResult:
    score: float
    dimensions: dict[str, float]
    needs_review: bool
    issues: list[str] = field(default_factory=list)


def completeness(inv: Invoice) -> float:
    present = 0
    for f in REQUIRED:
        v = getattr(inv, f)
        if f in ("supplier", "customer"):
            present += bool(v.name and v.cui)
        else:
            present += bool(v)
    return present / len(REQUIRED)


def line_math(lines) -> float:
    if not lines:
        return 1.0
    return sum(abs(l.quantity * l.unit_price - l.total) <= 0.01 for l in lines) / len(lines)


def plausibility(lines, rates=RATES) -> float:
    if not lines:
        return 1.0
    return sum(l.vat_rate in rates and l.quantity >= 0 and l.unit_price >= 0 for l in lines) / len(lines)


def result(dims: dict[str, float], weights: dict[str, float], threshold: float = 0.85) -> AuditResult:
    score = round(sum(weights[k] * v for k, v in dims.items()), 4)
    return AuditResult(score, dims, score < threshold, sorted(k for k, v in dims.items() if v < 1.0))


def audit_invoice(inv: Invoice) -> AuditResult:
    dims = {
        "completeness": completeness(inv),
        "line_math": line_math(inv.lines),
        "global_math": float(abs(sum(l.total for l in inv.lines) - inv.net_total) <= 0.05),
        "cui": sum(check_cui(p.cui).valid for p in (inv.supplier, inv.customer)) / 2,
        "plausibility": plausibility(inv.lines),
        "cross_reference": float(abs(inv.net_total + inv.vat_total - inv.gross_total) <= 0.05),
    }
    return result(dims, WEIGHTS)
