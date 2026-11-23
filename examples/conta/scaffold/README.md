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
| `conta.cui` | CUI (fiscal code) checks |
| `conta.audit` | confidence audit |
| `conta.export.saga` | SAGA XML export |
