"""Romanian fiscal identification code (CUI / CIF) checks."""

from __future__ import annotations

from dataclasses import dataclass

_KEY = "753217532"


@dataclass(frozen=True)
class CuiCheck:
    normalized: str
    valid: bool
    vat_payer: bool
    reason: str | None


def _checksum_ok(digits: str) -> bool:
    body, control = digits[:-1], int(digits[-1])
    key = _KEY[-len(body):]
    expected = sum(int(a) * int(b) for a, b in zip(body, key)) * 10 % 11
    return (0 if expected == 10 else expected) == control


def check_cui(raw: str) -> CuiCheck:
    s = "".join(str(raw).split()).upper()
    vat_payer = s.startswith("RO")
    if vat_payer:
        s = s[2:]
    if not s:
        return CuiCheck("", False, vat_payer, "empty")
    if not s.isdigit():
        return CuiCheck(s, False, vat_payer, "not_numeric")
    if not 2 <= len(s) <= 10:
        return CuiCheck(s, False, vat_payer, "bad_length")
    if not _checksum_ok(s):
        return CuiCheck(s, False, vat_payer, "bad_checksum")
    return CuiCheck(s, True, vat_payer, None)
