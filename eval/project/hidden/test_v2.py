"""Hidden acceptance tests for the conta v2 sprint (12 parallel tickets).

Never shown to the agents. Tests are named test_<ticket>_..., so per-ticket
results can be read from pytest's output.
"""

import json
import os
import subprocess
import sys
import xml.etree.ElementTree as ET
from dataclasses import fields
from datetime import date

import pytest

from conta.parse import parse_invoice

NS = {
    "inv": "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2",
    "cac": "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2",
    "cbc": "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2",
}
IBAN = "RO49AAAA1B31007593840000"  # standard valid example IBAN

INV = {
    "numar": "FCT-100",
    "data": "15.09.2025",
    "furnizor": {"denumire": "Dante International SA", "cui": "RO14399840", "iban": "RO49 AAAA 1B31 0075 9384 0000"},
    "client": {"denumire": "Tehno Expert SRL", "cui": "18547290"},
    "produse": [
        {"denumire": "Laptop", "cantitate": "1", "pret_unitar": "3.000,00", "cota_tva": "21", "valoare": "3.000,00"},
        {"denumire": "Carte", "cantitate": "2", "pret_unitar": "50,00", "cota_tva": "11", "valoare": "100,00"},
    ],
    "total_fara_tva": "3.100,00",
    "total_tva": "641,00",
    "total_plata": "3.741,00",
}
STORNO = dict(
    INV,
    numar="FCT-101",
    tip="Storno",
    produse=[{"denumire": "Laptop retur", "cantitate": "-1", "pret_unitar": "3.000,00", "cota_tva": "21", "valoare": "-3.000,00"}],
    total_fara_tva="-3.000,00",
    total_tva="-630,00",
    total_plata="-3.630,00",
)
EUR = dict(
    INV,
    numar="FCT-102",
    data="14.09.2025",  # a Sunday: the rate of Friday 12 September applies
    moneda="EUR",
    produse=[{"denumire": "Licenta", "cantitate": "2", "pret_unitar": "100,00", "cota_tva": "21", "valoare": "200,00"}],
    total_fara_tva="200,00",
    total_tva="42,00",
    total_plata="242,00",
)
# needs review at the strict threshold only (cui 0.5 -> score 0.925)
MEH = dict(INV, numar="FCT-103", client={"denumire": "X SRL", "cui": "18547291"})


# ---- doctype -----------------------------------------------------------------


def test_doctype_parsed():
    assert parse_invoice(INV).doc_type == "factura"
    assert parse_invoice(STORNO).doc_type == "storno"


def test_doctype_required_after_number():
    from conta.model import Invoice

    names = [f.name for f in fields(Invoice)]
    assert names[:2] == ["number", "doc_type"]
    assert next(f for f in fields(Invoice) if f.name == "doc_type").default is not None  # MISSING sentinel
    from dataclasses import MISSING

    assert next(f for f in fields(Invoice) if f.name == "doc_type").default is MISSING


# ---- storno-audit ------------------------------------------------------------


def test_storno_audit_plausible():
    from conta.audit import audit

    r = audit(parse_invoice(STORNO))
    assert r.dimensions["plausibility"] == 1.0
    assert r.score == 1.0


def test_storno_audit_normal_invoice_unchanged():
    from conta.audit import audit

    neg = dict(INV, produse=[dict(INV["produse"][0], cantitate="-1"), INV["produse"][1]])
    assert audit(parse_invoice(neg)).dimensions["plausibility"] == 0.5


# ---- saga-storno -------------------------------------------------------------


def test_saga_storno_flag():
    from conta.export.saga import export_saga

    s, n = export_saga(parse_invoice(STORNO)), export_saga(parse_invoice(INV))
    assert "<FacturaTip>S</FacturaTip>" in s
    assert "<FacturaTip>N</FacturaTip>" in n
    assert n.index("<FacturaNumar>") < n.index("<FacturaTip>")


# ---- fx ----------------------------------------------------------------------

RATES = "2025-09-11,EUR,5.0700\n\n2025-09-12,EUR,5.0800\n2025-09-12,USD,4.3300\n2025-09-15,EUR,5.0900\n"


def test_fx_rate_lookup():
    from conta.fx import RateTable

    t = RateTable.from_csv(RATES)
    assert t.rate("RON", date(2025, 9, 14)) == 1.0
    assert t.rate("EUR", date(2025, 9, 14)) == 5.08
    assert t.rate("EUR", date(2025, 9, 15)) == 5.09
    with pytest.raises(KeyError):
        t.rate("EUR", date(2025, 9, 1))


def test_fx_to_ron():
    from conta.fx import RateTable, to_ron

    inv = parse_invoice(EUR)
    ron = to_ron(inv, RateTable.from_csv(RATES))
    assert ron.currency == "RON" and inv.currency == "EUR"
    assert (ron.net_total, ron.vat_total, ron.gross_total) == (1016.0, 213.36, 1229.36)
    assert (ron.lines[0].unit_price, ron.lines[0].total, ron.lines[0].vat_amount) == (508.0, 1016.0, 213.36)
    assert inv.net_total == 200.0


# ---- iban --------------------------------------------------------------------


def test_iban_check():
    from conta.iban import check_iban

    c = check_iban("ro49 aaaa 1b31 0075 9384 0000")
    assert (c.normalized, c.valid, c.bank) == (IBAN, True, "AAAA")
    bad = check_iban("RO49AAAA1B31007593840001")
    assert bad.valid is False and bad.bank == ""
    assert check_iban("DE89370400440532013000").valid is False


def test_iban_on_party():
    from conta.model import Party

    assert parse_invoice(INV).supplier.iban.replace(" ", "") == IBAN
    assert parse_invoice(INV).customer.iban == ""
    assert Party(name="x", cui="1").iban == ""


# ---- efactura-iban -----------------------------------------------------------


def test_efactura_payment_means():
    from conta.export.efactura import export_efactura

    root = ET.fromstring(export_efactura(parse_invoice(INV)))
    pm = root.find("cac:PaymentMeans", NS)
    assert pm is not None
    assert pm.findtext("cbc:PaymentMeansCode", namespaces=NS) == "42"
    assert pm.findtext("cac:PayeeFinancialAccount/cbc:ID", namespaces=NS) == IBAN


def test_efactura_no_payment_means_without_valid_iban():
    from conta.export.efactura import export_efactura

    d = dict(INV, furnizor={"denumire": "Dante International SA", "cui": "RO14399840", "iban": "RO49AAAA1B31007593840001"})
    root = ET.fromstring(export_efactura(parse_invoice(d)))
    assert root.find("cac:PaymentMeans", NS) is None


# ---- report ------------------------------------------------------------------


def test_report_summarize():
    from conta.report import summarize

    s = summarize([parse_invoice(INV), parse_invoice(MEH), parse_invoice(STORNO)])
    assert s["count"] == 3
    assert (s["net"], s["vat"], s["gross"]) == (3200.0, 652.0, 3852.0)
    assert s["by_supplier"] == {"14399840": 3852.0}
    assert s["by_rate"] == {21.0: 3000.0, 11.0: 200.0}
    assert s["needs_review"] == 0


# ---- cli-summary -------------------------------------------------------------


@pytest.fixture
def indir(tmp_path):
    d = tmp_path / "in"
    d.mkdir()
    for name, data in {"a": INV, "b": MEH}.items():
        (d / f"{name}.json").write_text(json.dumps(data))
    return d


def test_cli_summary_function(indir, tmp_path):
    from conta.cli import process

    out = tmp_path / "out"
    process(indir, out, "saga", summary=True)
    s = json.loads((out / "summary.json").read_text())
    assert s["count"] == 2 and s["gross"] == 7482.0
    assert s["by_supplier"] == {"14399840": 7482.0}
    assert s["by_rate"] == {"21.0": 6000.0, "11.0": 200.0}
    assert s["needs_review"] == 0


def test_cli_no_summary_by_default(indir, tmp_path):
    from conta.cli import process

    out = tmp_path / "out"
    process(indir, out)
    assert not (out / "summary.json").exists()


# ---- audit-policy ------------------------------------------------------------


def test_audit_policy():
    from conta.audit import DEFAULT, STRICT, AuditPolicy, audit

    meh = parse_invoice(MEH)
    assert DEFAULT.threshold == 0.85 and STRICT.threshold == 0.95
    assert audit(meh).needs_review is False
    assert audit(meh, policy=STRICT).needs_review is True
    assert audit(meh, policy=AuditPolicy(threshold=0.9)).needs_review is False
    with pytest.raises(TypeError):
        audit(meh, strict=True)


# ---- cli-strict --------------------------------------------------------------


def test_cli_strict_function(indir, tmp_path):
    from conta.cli import process

    rep = {e["file"]: e for e in process(indir, tmp_path / "o1", strict=True)}
    assert rep["b.json"]["needs_review"] is True and rep["a.json"]["needs_review"] is False
    rep = {e["file"]: e for e in process(indir, tmp_path / "o2")}
    assert rep["b.json"]["needs_review"] is False


def test_cli_flags_module(indir, tmp_path):
    out = tmp_path / "o3"
    r = subprocess.run(
        [sys.executable, "-m", "conta", "process", str(indir), str(out), "--strict", "--summary"],
        capture_output=True,
        text=True,
        env=dict(os.environ),
    )
    assert r.returncode == 0, r.stderr
    rep = {e["file"]: e for e in json.loads((out / "report.json").read_text())}
    assert rep["b.json"]["needs_review"] is True
    assert (out / "summary.json").exists()


# ---- dedup -------------------------------------------------------------------


def test_dedup():
    from conta.dedup import find_duplicates

    a = parse_invoice(INV)
    b = parse_invoice(dict(INV, numar=" fct-100 ", furnizor={"denumire": "Dante", "cui": "14399840"}))
    c = parse_invoice(MEH)
    d = parse_invoice(dict(MEH, numar="FCT-103"))
    e = parse_invoice(dict(INV, furnizor={"denumire": "Other SRL", "cui": "18547290"}))
    assert find_duplicates({"z": a, "y": b, "c": c, "d": d, "e": e}) == [["c", "d"], ["y", "z"]]
    assert find_duplicates({"a": a}) == []


# ---- review-reasons ----------------------------------------------------------


def test_review_reasons():
    from conta.review import review_queue

    bad = parse_invoice(dict(INV, total_fara_tva="2.000,00", total_tva="1,00", total_plata="5,00"))
    q = review_queue({"b": bad, "g": parse_invoice(INV)})
    assert [i.doc_id for i in q] == ["b"]
    assert q[0].issues == ["cross_reference", "global_math"]
    assert q[0].reasons == ["net + VAT does not equal the gross total", "line totals do not add up to the net total"]
