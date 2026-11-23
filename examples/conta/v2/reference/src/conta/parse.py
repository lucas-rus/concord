"""Turn extracted document data (JSON with Romanian keys, as returned by the
OCR/LLM stage) into domain objects."""

from __future__ import annotations

from datetime import date, datetime
from typing import Any

from conta.model import Invoice, LineItem, Party
from conta.receipt import Receipt


def sf(val: Any) -> float:
    """Parse a number as printed on Romanian documents ("1.234,56", "12,5", 7)."""
    if val is None or val == "":
        return 0.0
    if isinstance(val, (int, float)):
        return float(val)
    s = str(val).strip().replace(" ", "").replace("lei", "").replace("RON", "")
    if "," in s and "." in s:
        s = s.replace(".", "").replace(",", ".")
    elif "," in s:
        s = s.replace(",", ".")
    try:
        return float(s)
    except ValueError:
        return 0.0


def parse_date(val: str) -> date:
    """Parse DD.MM.YYYY (also accepts DD/MM/YYYY and ISO YYYY-MM-DD)."""
    val = val.strip()
    for fmt in ("%d.%m.%Y", "%d/%m/%Y", "%Y-%m-%d"):
        try:
            return datetime.strptime(val, fmt).date()
        except ValueError:
            continue
    raise ValueError(f"unrecognised date: {val!r}")


def _party(d: dict[str, Any] | None) -> Party:
    d = d or {}
    return Party(
        name=str(d.get("denumire", "")).strip(),
        cui=str(d.get("cui", "")).strip(),
        address=str(d.get("adresa", "")).strip(),
        iban=str(d.get("iban", "")).strip(),
    )


def _vat_amount(p: dict[str, Any]) -> float:
    if p.get("valoare_tva") not in (None, ""):
        return sf(p.get("valoare_tva"))
    return round(sf(p.get("valoare")) * sf(p.get("cota_tva")) / 100, 2)


def _lines(data: dict[str, Any]) -> list[LineItem]:
    return [
        LineItem(
            description=str(p.get("denumire", "")).strip(),
            quantity=sf(p.get("cantitate")),
            unit_price=sf(p.get("pret_unitar")),
            vat_rate=sf(p.get("cota_tva")),
            total=sf(p.get("valoare")),
            vat_amount=_vat_amount(p),
        )
        for p in data.get("produse", [])
    ]


def parse_receipt(data: dict[str, Any]) -> Receipt:
    return Receipt(
        number=str(data.get("nr_bon", "")).strip(),
        issue_date=parse_date(str(data.get("data", ""))),
        supplier=_party(data.get("emitent")),
        lines=_lines(data),
        total=sf(data.get("total")),
        payment=str(data.get("plata", "numerar")).strip().lower(),
    )


def parse_invoice(data: dict[str, Any]) -> Invoice:
    """Build an Invoice from extracted data."""
    lines = [
        LineItem(
            description=str(p.get("denumire", "")).strip(),
            quantity=sf(p.get("cantitate")),
            unit_price=sf(p.get("pret_unitar")),
            vat_rate=sf(p.get("cota_tva")),
            total=sf(p.get("valoare")),
            vat_amount=_vat_amount(p),
        )
        for p in data.get("produse", [])
    ]
    return Invoice(
        number=str(data.get("numar", "")).strip(),
        doc_type="storno" if str(data.get("tip", "")).strip().lower() == "storno" else "factura",
        issue_date=parse_date(str(data.get("data", ""))),
        supplier=_party(data.get("furnizor")),
        customer=_party(data.get("client")),
        lines=lines,
        net_total=sf(data.get("total_fara_tva")),
        vat_total=sf(data.get("total_tva")),
        gross_total=sf(data.get("total_plata")),
        currency=str(data.get("moneda", "RON")).strip().upper() or "RON",
    )
