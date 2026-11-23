import json

from conta.cli import process
from conta.export.efactura import export_efactura
from conta.parse import parse_invoice, parse_receipt
from conta.review import review_queue
from conta.vat import valid_rates, vat_breakdown


def test_efactura(invoice_data):
    assert "Invoice-2" in export_efactura(parse_invoice(invoice_data))


def test_vat(invoice_data):
    inv = parse_invoice(invoice_data)
    assert 19.0 in valid_rates(inv.issue_date)
    assert vat_breakdown(inv)[19.0].net == 3300.0


def test_receipt_and_review(invoice_data):
    r = parse_receipt({"nr_bon": "1", "data": "01.02.2025", "emitent": {"denumire": "M SRL", "cui": "RO6719278"},
                       "produse": [{"denumire": "x", "cantitate": "1", "pret_unitar": "2", "cota_tva": "9", "valoare": "2"}],
                       "total": "2,18", "plata": "card"})
    assert review_queue({"i": parse_invoice(invoice_data), "r": r}) == []


def test_cli(invoice_data, tmp_path):
    (tmp_path / "in").mkdir()
    (tmp_path / "in" / "a.json").write_text(json.dumps(invoice_data))
    report = process(tmp_path / "in", tmp_path / "out")
    assert report[0]["exported"] is True
