from datetime import date

from conta.audit import audit, completeness
from conta.cui import check_cui
from conta.export.saga import export_saga
from conta.parse import parse_invoice, sf


def test_sf_romanian_numbers():
    assert sf("1.234,56") == 1234.56
    assert sf("12,5") == 12.5
    assert sf(7) == 7.0
    assert sf("") == 0.0


def test_parse_invoice(invoice_data):
    inv = parse_invoice(invoice_data)
    assert inv.number == "FCT-1042"
    assert inv.issue_date == date(2025, 3, 14)
    assert inv.supplier.cui == "14399840"
    assert len(inv.lines) == 2
    assert inv.gross_total == 3927.0
    assert inv.lines[0].vat_amount == 608.0


def test_cui():
    assert check_cui("RO 14399840").valid
    assert not check_cui("14399841").valid


def test_audit(invoice_data):
    inv = parse_invoice(invoice_data)
    assert completeness(inv) == 1.0
    assert audit(inv).score == 1.0


def test_saga_export(invoice_data):
    xml = export_saga(parse_invoice(invoice_data))
    assert "<FurnizorCIF>14399840</FurnizorCIF>" in xml
    assert xml.count("<Linie>") == 2
