"""Human review queue."""

from __future__ import annotations

from dataclasses import dataclass

from conta.audit import audit
from conta.model import Invoice
from conta.receipt import Receipt


@dataclass
class ReviewItem:
    doc_id: str
    score: float
    issues: list[str]
    priority: str
    reasons: list[str]


REASONS = {
    "completeness": "missing required fields",
    "line_math": "line totals do not match quantity x price",
    "global_math": "line totals do not add up to the net total",
    "cui": "invalid fiscal code (CUI)",
    "plausibility": "implausible VAT rate or negative amounts",
    "cross_reference": "net + VAT does not equal the gross total",
}


def _priority(score: float) -> str:
    return "high" if score < 0.6 else "medium" if score < 0.75 else "low"


def review_queue(docs: dict[str, Invoice | Receipt]) -> list[ReviewItem]:
    items = []
    for doc_id, doc in docs.items():
        r = audit(doc)
        if r.needs_review:
            items.append(ReviewItem(doc_id, r.score, r.issues, _priority(r.score), [REASONS[i] for i in r.issues]))
    return sorted(items, key=lambda i: (i.score, i.doc_id))
