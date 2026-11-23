"""Export to e-Factura (UBL 2.1)."""

from __future__ import annotations

import xml.etree.ElementTree as ET

from conta.cui import check_cui
from conta.model import Invoice, Party

NS_INV = "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"
NS_CAC = "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2"
NS_CBC = "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2"
ET.register_namespace("", NS_INV)
ET.register_namespace("cac", NS_CAC)
ET.register_namespace("cbc", NS_CBC)


class ExportError(Exception):
    pass


def _cac(parent, tag):
    return ET.SubElement(parent, f"{{{NS_CAC}}}{tag}")


def _cbc(parent, tag, text):
    e = ET.SubElement(parent, f"{{{NS_CBC}}}{tag}")
    e.text = str(text)
    return e


def _party(parent, tag, p: Party):
    c = check_cui(p.cui)
    party = _cac(_cac(parent, tag), "Party")
    _cbc(_cac(party, "PartyName"), "Name", p.name)
    _cbc(_cac(party, "PartyTaxScheme"), "CompanyID", ("RO" if c.vat_payer else "") + c.normalized)


def export_efactura(inv: Invoice) -> str:
    if not check_cui(inv.supplier.cui).valid:
        raise ExportError(f"invalid supplier CUI {inv.supplier.cui!r}")
    root = ET.Element(f"{{{NS_INV}}}Invoice")
    _cbc(root, "ID", inv.number)
    _cbc(root, "IssueDate", inv.issue_date.isoformat())
    _cbc(root, "DocumentCurrencyCode", inv.currency)
    _party(root, "AccountingSupplierParty", inv.supplier)
    _party(root, "AccountingCustomerParty", inv.customer)
    _cbc(_cac(root, "TaxTotal"), "TaxAmount", f"{inv.vat_total:.2f}")
    lmt = _cac(root, "LegalMonetaryTotal")
    _cbc(lmt, "TaxExclusiveAmount", f"{inv.net_total:.2f}")
    _cbc(lmt, "TaxInclusiveAmount", f"{inv.gross_total:.2f}")
    _cbc(lmt, "PayableAmount", f"{inv.gross_total:.2f}")
    for i, l in enumerate(inv.lines, 1):
        line = _cac(root, "InvoiceLine")
        _cbc(line, "ID", i)
        _cbc(line, "InvoicedQuantity", f"{l.quantity:.2f}")
        _cbc(line, "LineExtensionAmount", f"{l.total:.2f}")
        item = _cac(line, "Item")
        _cbc(item, "Name", l.description)
        _cbc(_cac(item, "ClassifiedTaxCategory"), "Percent", f"{l.vat_rate:.2f}")
    return ET.tostring(root, encoding="unicode")
