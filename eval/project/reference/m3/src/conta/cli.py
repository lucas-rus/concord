"""Batch processing of extracted invoices."""

from __future__ import annotations

import json
from pathlib import Path

from conta.audit import audit
from conta.export.efactura import ExportError, export_efactura
from conta.export.saga import export_saga
from conta.parse import parse_invoice


def process(in_dir: Path, out_dir: Path, fmt: str = "saga") -> list[dict]:
    out_dir.mkdir(parents=True, exist_ok=True)
    report = []
    for f in sorted(Path(in_dir).glob("*.json")):
        inv = parse_invoice(json.loads(f.read_text()))
        r = audit(inv)
        try:
            xml = export_efactura(inv) if fmt == "efactura" else export_saga(inv)
            (out_dir / f"{f.stem}.xml").write_text(xml)
            exported = True
        except ExportError:
            exported = False
        report.append({"file": f.name, "score": r.score, "needs_review": r.needs_review, "exported": exported})
    (out_dir / "report.json").write_text(json.dumps(report, indent=2))
    return report
