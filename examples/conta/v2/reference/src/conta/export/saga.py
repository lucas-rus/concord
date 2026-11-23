"""Export to the SAGA accounting software's XML import format (simplified)."""

from __future__ import annotations

import xml.etree.ElementTree as ET

from conta.model import Invoice


def _num(x: float) -> str:
    return f"{x:.2f}"


def export_saga(inv: Invoice) -> str:
    """One <Factura> with header and lines."""
    root = ET.Element("Facturi")
    f = ET.SubElement(root, "Factura")
    antet = ET.SubElement(f, "Antet")
    ET.SubElement(antet, "FurnizorNume").text = inv.supplier.name
    ET.SubElement(antet, "FurnizorCIF").text = inv.supplier.cui
    ET.SubElement(antet, "ClientNume").text = inv.customer.name
    ET.SubElement(antet, "ClientCIF").text = inv.customer.cui
    ET.SubElement(antet, "FacturaNumar").text = inv.number
    ET.SubElement(antet, "FacturaTip").text = "S" if inv.doc_type == "storno" else "N"
    ET.SubElement(antet, "FacturaData").text = inv.issue_date.strftime("%d.%m.%Y")
    ET.SubElement(antet, "FacturaMoneda").text = inv.currency
    ET.SubElement(antet, "TotalFaraTVA").text = _num(inv.net_total)
    ET.SubElement(antet, "TotalTVA").text = _num(inv.vat_total)
    ET.SubElement(antet, "Total").text = _num(inv.gross_total)
    detalii = ET.SubElement(f, "Detalii")
    for i, ln in enumerate(inv.lines, 1):
        linie = ET.SubElement(detalii, "Linie")
        ET.SubElement(linie, "LinieNrCrt").text = str(i)
        ET.SubElement(linie, "Descriere").text = ln.description
        ET.SubElement(linie, "Cantitate").text = _num(ln.quantity)
        ET.SubElement(linie, "Pret").text = _num(ln.unit_price)
        ET.SubElement(linie, "Valoare").text = _num(ln.total)
        ET.SubElement(linie, "CotaTVA").text = _num(ln.vat_rate)
        ET.SubElement(linie, "TVA").text = _num(ln.vat_amount)
    return ET.tostring(root, encoding="unicode")
