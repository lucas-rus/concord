"""Hidden acceptance tests for milestone 2 (never shown to the agents)."""

from datetime import date

import pytest

from conta.parse import parse_invoice

INV = {
    "numar": "FCT-90",
    "data": "15.09.2025",
    "furnizor": {"denumire": "Dante International SA", "cui": "RO14399840"},
    "client": {"denumire": "Tehno Expert SRL", "cui": "18547290"},
    "produse": [
        {"denumire": "Carte", "cantitate": "2", "pret_unitar": "50,00", "cota_tva": "11", "valoare": "100,00"},
        {"denumire": "Laptop", "cantitate": "1", "pret_unitar": "3.000,00", "cota_tva": "21", "valoare": "3.000,00", "valoare_tva": "630,00"},
        {"denumire": "Husa", "cantitate": "1", "pret_unitar": "40,00", "cota_tva": "21", "valoare": "40,00"},
    ],
    "total_fara_tva": "3.140,00",
    "total_tva": "649,40",
    "total_plata": "3.789,40",
}

BON = {
    "nr_bon": "0042",
    "data": "02.09.2025",
    "emitent": {"denumire": "Mega Image SRL", "cui": "RO6719278"},
    "produse": [
        {"denumire": "Paine", "cantitate": "2", "pret_unitar": "4,50", "cota_tva": "11", "valoare": "9,00"},
        {"denumire": "Lapte", "cantitate": "1", "pret_unitar": "7,00", "cota_tva": "11", "valoare": "7,00"},
    ],
    "total": "17,76",
    "plata": "Card",
}


# ---- vat ---------------------------------------------------------------------


def test_valid_rates_change_on_1_august_2025():
    from conta.vat import valid_rates

    assert valid_rates(date(2025, 7, 31)) == frozenset({0.0, 5.0, 9.0, 19.0})
    assert valid_rates(date(2025, 8, 1)) == frozenset({0.0, 11.0, 21.0})


def test_vat_breakdown():
    from conta.vat import VatGroup, vat_breakdown

    b = vat_breakdown(parse_invoice(INV))
    assert b == {11.0: VatGroup(net=100.0, vat=11.0), 21.0: VatGroup(net=3040.0, vat=638.4)}


def test_plausibility_is_date_aware():
    from conta.audit import audit_invoice

    assert audit_invoice(parse_invoice(INV)).dimensions["plausibility"] == 1.0
    old = dict(INV, data="15.07.2025")
    assert audit_invoice(parse_invoice(old)).dimensions["plausibility"] == 0.0


# ---- lines -------------------------------------------------------------------


def test_line_vat_amount_parsed_or_computed():
    inv = parse_invoice(INV)
    assert [ln.vat_amount for ln in inv.lines] == [11.0, 630.0, 8.4]


def test_lineitem_requires_vat_amount():
    from conta.model import LineItem

    with pytest.raises(TypeError):
        LineItem(description="x", quantity=1, unit_price=1, vat_rate=19, total=1)  # type: ignore[call-arg]
    li = LineItem(description="x", quantity=1, unit_price=1, vat_rate=19, total=1, vat_amount=0.19)
    assert li.vat_amount == 0.19


def test_saga_vat_elements():
    from conta.export.saga import export_saga

    xml = export_saga(parse_invoice(INV))
    assert "<TVA>630.00</TVA>" in xml
    assert "<TotalFaraTVA>3140.00</TotalFaraTVA>" in xml
    assert "<TotalTVA>649.40</TotalTVA>" in xml
    assert "<Total>3789.40</Total>" in xml
    assert xml.index("<CotaTVA>") < xml.index("<TVA>")


# ---- receipts ----------------------------------------------------------------


def test_parse_receipt():
    from conta.parse import parse_receipt

    r = parse_receipt(BON)
    assert r.number == "0042"
    assert r.issue_date == date(2025, 9, 2)
    assert r.supplier.cui == "RO6719278"
    assert r.payment == "card"
    assert r.total == pytest.approx(17.76)
    assert [ln.total for ln in r.lines] == [9.0, 7.0]


def test_audit_receipt():
    from conta.audit import audit_receipt
    from conta.parse import parse_receipt

    r = audit_receipt(parse_receipt(BON))
    assert set(r.dimensions) == {"completeness", "line_math", "cui", "plausibility"}
    assert r.dimensions["cui"] == 1.0
    assert r.score == 1.0 and r.needs_review is False and r.issues == []


def test_audit_receipt_bad_line():
    from conta.audit import audit_receipt
    from conta.parse import parse_receipt

    bad = dict(BON, produse=[dict(BON["produse"][0], valoare="10,00"), BON["produse"][1]])
    r = audit_receipt(parse_receipt(bad))
    assert r.dimensions["line_math"] == 0.5
    assert r.score == pytest.approx(0.825, abs=1e-4)
    assert r.needs_review is True and r.issues == ["line_math"]
