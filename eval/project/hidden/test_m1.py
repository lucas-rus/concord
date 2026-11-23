"""Hidden acceptance tests for milestone 1 (never shown to the agents)."""

import xml.etree.ElementTree as ET
from datetime import date

import pytest

from conta.model import Invoice, LineItem, Party
from conta.parse import parse_invoice

NS = {
    "inv": "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2",
    "cac": "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2",
    "cbc": "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2",
}

DATA = {
    "numar": "FCT-7",
    "data": "10.06.2025",
    "furnizor": {"denumire": "Dante International SA", "cui": "RO14399840"},
    "client": {"denumire": "Tehno Expert SRL", "cui": "18547290"},
    "produse": [
        {"denumire": "Monitor", "cantitate": "2", "pret_unitar": "500,00", "cota_tva": "19", "valoare": "1.000,00"},
        {"denumire": "Cablu", "cantitate": "4", "pret_unitar": "12,50", "cota_tva": "19", "valoare": "50,00"},
    ],
    "total_fara_tva": "1.050,00",
    "total_tva": "199,50",
    "total_plata": "1.249,50",
}


def inv(**over):
    d = {**DATA, **over}
    return parse_invoice(d)


# ---- cui ---------------------------------------------------------------------


def test_check_cui_valid_plain():
    from conta.cui import check_cui

    c = check_cui("14399840")
    assert (c.normalized, c.valid, c.vat_payer, c.reason) == ("14399840", True, False, None)


def test_check_cui_ro_prefix_and_spaces():
    from conta.cui import check_cui

    c = check_cui(" ro 1439 9840 ")
    assert (c.normalized, c.valid, c.vat_payer, c.reason) == ("14399840", True, True, None)


@pytest.mark.parametrize(
    "raw,reason",
    [("", "empty"), ("   ", "empty"), ("RO12A45", "not_numeric"), ("1", "bad_length"), ("12345678901", "bad_length"), ("14399841", "bad_checksum")],
)
def test_check_cui_reasons(raw, reason):
    from conta.cui import check_cui

    c = check_cui(raw)
    assert c.valid is False and c.reason == reason


def test_check_cui_is_frozen():
    from conta.cui import check_cui

    c = check_cui("14399840")
    with pytest.raises(Exception):
        c.valid = False  # type: ignore[misc]


def test_single_entry_point():
    import conta.cui

    assert not hasattr(conta.cui, "is_valid_cui")


# ---- audit -------------------------------------------------------------------


def test_audit_perfect_invoice():
    from conta.audit import audit_invoice

    r = audit_invoice(inv())
    assert r.score == 1.0
    assert r.needs_review is False
    assert r.issues == []
    assert set(r.dimensions) == {"completeness", "line_math", "global_math", "cui", "plausibility", "cross_reference"}


def test_audit_ro_prefixed_cui_counts_as_valid():
    from conta.audit import audit_invoice

    assert audit_invoice(inv()).dimensions["cui"] == 1.0


def test_audit_bad_line_and_cui():
    from conta.audit import audit_invoice

    bad = dict(DATA)
    bad["produse"] = [dict(DATA["produse"][0], valoare="999,00"), DATA["produse"][1]]
    bad["client"] = {"denumire": "X SRL", "cui": "18547291"}
    r = audit_invoice(parse_invoice(bad))
    assert r.dimensions["line_math"] == 0.5
    assert r.dimensions["cui"] == 0.5
    assert r.dimensions["global_math"] == 0.0
    # 0.2*1 + 0.25*0.5 + 0.2*0 + 0.15*0.5 + 0.1*1 + 0.1*1
    assert r.score == pytest.approx(0.6, abs=1e-4)
    assert r.needs_review is True
    assert r.issues == ["cui", "global_math", "line_math"]


def test_audit_implausible_rate():
    from conta.audit import audit_invoice

    d = dict(DATA)
    d["produse"] = [dict(DATA["produse"][0], cota_tva="24"), DATA["produse"][1]]
    assert audit_invoice(parse_invoice(d)).dimensions["plausibility"] == 0.5


# ---- e-Factura ---------------------------------------------------------------


def _root(xml: str) -> ET.Element:
    root = ET.fromstring(xml)
    assert root.tag == "{%s}Invoice" % NS["inv"]
    return root


def test_efactura_header_and_parties():
    from conta.export.efactura import export_efactura

    root = _root(export_efactura(inv()))
    assert root.findtext("cbc:ID", namespaces=NS) == "FCT-7"
    assert root.findtext("cbc:IssueDate", namespaces=NS) == "2025-06-10"
    assert root.findtext("cbc:DocumentCurrencyCode", namespaces=NS) == "RON"
    sup = root.find("cac:AccountingSupplierParty/cac:Party", NS)
    cus = root.find("cac:AccountingCustomerParty/cac:Party", NS)
    assert sup.findtext("cac:PartyName/cbc:Name", namespaces=NS) == "Dante International SA"
    assert sup.findtext("cac:PartyTaxScheme/cbc:CompanyID", namespaces=NS) == "RO14399840"
    assert cus.findtext("cac:PartyTaxScheme/cbc:CompanyID", namespaces=NS) == "18547290"


def test_efactura_lines_and_totals():
    from conta.export.efactura import export_efactura

    root = _root(export_efactura(inv()))
    lines = root.findall("cac:InvoiceLine", NS)
    assert [l.findtext("cbc:ID", namespaces=NS) for l in lines] == ["1", "2"]
    assert lines[0].findtext("cbc:LineExtensionAmount", namespaces=NS) == "1000.00"
    assert lines[1].findtext("cac:Item/cbc:Name", namespaces=NS) == "Cablu"
    assert float(lines[1].findtext("cac:Item/cac:ClassifiedTaxCategory/cbc:Percent", namespaces=NS)) == 19.0
    assert root.findtext("cac:TaxTotal/cbc:TaxAmount", namespaces=NS) == "199.50"
    lmt = root.find("cac:LegalMonetaryTotal", NS)
    assert lmt.findtext("cbc:TaxExclusiveAmount", namespaces=NS) == "1050.00"
    assert lmt.findtext("cbc:PayableAmount", namespaces=NS) == "1249.50"


def test_efactura_spaced_lowercase_prefix():
    from conta.export.efactura import export_efactura

    d = dict(DATA, furnizor={"denumire": "Dante International SA", "cui": "ro 14399840"})
    root = _root(export_efactura(parse_invoice(d)))
    assert root.findtext("cac:AccountingSupplierParty/cac:Party/cac:PartyTaxScheme/cbc:CompanyID", namespaces=NS) == "RO14399840"


def test_efactura_invalid_supplier_cui():
    from conta.export.efactura import ExportError, export_efactura

    d = dict(DATA, furnizor={"denumire": "Fake SRL", "cui": "RO14399841"})
    with pytest.raises(ExportError):
        export_efactura(parse_invoice(d))
