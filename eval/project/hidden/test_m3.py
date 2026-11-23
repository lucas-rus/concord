"""Hidden acceptance tests for milestone 3 and the finished project (never shown to the agents)."""

import json
import subprocess
import sys
import os
from pathlib import Path

import pytest

from conta.parse import parse_invoice, parse_receipt

GOOD = {
    "numar": "FCT-1",
    "data": "15.09.2025",
    "furnizor": {"denumire": "Dante International SA", "cui": "RO14399840"},
    "client": {"denumire": "Tehno Expert SRL", "cui": "18547290"},
    "produse": [{"denumire": "Laptop", "cantitate": "1", "pret_unitar": "3.000,00", "cota_tva": "21", "valoare": "3.000,00"}],
    "total_fara_tva": "3.000,00",
    "total_tva": "630,00",
    "total_plata": "3.630,00",
}
# global_math and cross_reference broken: score 0.7
BAD = dict(GOOD, numar="FCT-2", total_fara_tva="2.000,00", total_tva="1,00", total_plata="5,00")
# one CUI invalid: score 0.925
MEH = dict(GOOD, numar="FCT-3", client={"denumire": "X SRL", "cui": "18547291"})
# invalid supplier CUI: cannot be exported to e-Factura; cui 0, score 0.85 -> no review in default mode
NOEXP = dict(GOOD, numar="FCT-4", furnizor={"denumire": "Fake SRL", "cui": "RO14399841"}, client={"denumire": "Y SRL", "cui": "14399841"})
BON = {
    "nr_bon": "7",
    "data": "02.09.2025",
    "emitent": {"denumire": "Mega Image SRL", "cui": "RO6719278"},
    # one of two lines wrong: line_math 0.5, score 0.825 (receipt weights)
    "produse": [
        {"denumire": "Paine", "cantitate": "2", "pret_unitar": "4,50", "cota_tva": "11", "valoare": "10,00"},
        {"denumire": "Lapte", "cantitate": "1", "pret_unitar": "7,00", "cota_tva": "11", "valoare": "7,00"},
    ],
    "total": "18,87",
    "plata": "numerar",
}


# ---- unify -------------------------------------------------------------------


def test_audit_dispatches():
    from conta.audit import audit

    assert audit(parse_invoice(GOOD)).score == 1.0
    r = audit(parse_receipt(BON))
    assert set(r.dimensions) == {"completeness", "line_math", "cui", "plausibility"}


def test_audit_strict_threshold():
    from conta.audit import audit

    meh = parse_invoice(MEH)
    assert audit(meh).score == pytest.approx(0.925, abs=1e-4)
    assert audit(meh).needs_review is False
    assert audit(meh, strict=True).needs_review is True
    assert audit(meh, strict=True).score == audit(meh).score


def test_old_audit_functions_removed():
    import conta.audit

    assert not hasattr(conta.audit, "audit_invoice")
    assert not hasattr(conta.audit, "audit_receipt")


# ---- review ------------------------------------------------------------------


def test_review_queue():
    from conta.review import review_queue

    docs = {"b": parse_invoice(BAD), "g": parse_invoice(GOOD), "m": parse_invoice(MEH), "r": parse_receipt(BON)}
    q = review_queue(docs)
    assert [i.doc_id for i in q] == ["b", "r"]
    assert q[0].score == pytest.approx(0.7, abs=1e-4)
    assert q[1].score == pytest.approx(0.825, abs=1e-4)
    assert q[0].priority == "medium"
    assert q[0].issues == ["cross_reference", "global_math"]
    assert q[1].priority == "low"


# ---- cli ---------------------------------------------------------------------


@pytest.fixture
def indir(tmp_path):
    d = tmp_path / "in"
    d.mkdir()
    for name, data in {"a_good": GOOD, "b_bad": BAD, "c_noexp": NOEXP}.items():
        (d / f"{name}.json").write_text(json.dumps(data))
    return d


def test_cli_process_function_saga(indir, tmp_path):
    from conta.cli import process

    out = tmp_path / "out"
    report = process(indir, out, "saga")
    assert [e["file"] for e in report] == ["a_good.json", "b_bad.json", "c_noexp.json"]
    assert all(e["exported"] for e in report)
    assert (out / "a_good.xml").exists() and (out / "c_noexp.xml").exists()
    assert json.loads((out / "report.json").read_text()) == report
    assert report[1]["needs_review"] is True and report[0]["needs_review"] is False


def test_cli_module_efactura(indir, tmp_path):
    out = tmp_path / "out2"
    env = dict(os.environ)
    r = subprocess.run(
        [sys.executable, "-m", "conta", "process", str(indir), str(out), "--format", "efactura"],
        capture_output=True,
        text=True,
        env=env,
    )
    assert r.returncode == 0, r.stderr
    report = json.loads((out / "report.json").read_text())
    by = {e["file"]: e for e in report}
    assert by["a_good.json"]["exported"] is True
    assert by["c_noexp.json"]["exported"] is False
    assert not (out / "c_noexp.xml").exists()
    assert "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2" in (out / "a_good.xml").read_text()
