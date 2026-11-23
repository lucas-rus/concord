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


def _priority(score: float) -> str:
    return "high" if score < 0.6 else "medium" if score < 0.75 else "low"


def review_queue(docs: dict[str, Invoice | Receipt]) -> list[ReviewItem]:
    items = []
    for doc_id, doc in docs.items():
        r = audit(doc)
        if r.needs_review:
            items.append(ReviewItem(doc_id, r.score, r.issues, _priority(r.score)))
    return sorted(items, key=lambda i: (i.score, i.doc_id))
