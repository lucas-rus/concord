# conta

Offline processing of Romanian fiscal documents. The OCR/LLM stage (not part
of this package) turns a scanned document into JSON with Romanian keys; this
package takes it from there:

    extracted JSON --parse--> Invoice --audit--> confidence --export--> SAGA / e-Factura XML

Layout: `src/conta/` (package), `tests/` (pytest; run with `PYTHONPATH=src`).

| module | role |
|---|---|
| `conta.model` | `Party`, `LineItem`, `Invoice` |
| `conta.parse` | `parse_invoice(data)`, number/date helpers |
| `conta.cui` | `check_cui`: CUI (fiscal code) checks |
| `conta.receipt` | `Receipt` (bon fiscal) |
| `conta.vat` | date-aware VAT rates, VAT breakdown |
| `conta.audit` | `audit(doc, *, strict=False)`: confidence audit of invoices and receipts |
| `conta.review` | human review queue |
| `conta.export.saga` | SAGA XML export |
| `conta.export.efactura` | e-Factura (UBL 2.1) export |
| `conta.cli` / `python -m conta` | batch processing |
