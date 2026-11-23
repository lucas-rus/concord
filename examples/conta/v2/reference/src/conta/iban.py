"""Bank account (IBAN) checks."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class IbanCheck:
    normalized: str
    valid: bool
    bank: str


def _mod97_ok(iban: str) -> bool:
    s = iban[4:] + iban[:4]
    digits = "".join(str(int(c, 36)) for c in s)
    return digits.isdigit() and int(digits) % 97 == 1


def check_iban(raw: str) -> IbanCheck:
    s = "".join(str(raw).split()).upper()
    valid = s.startswith("RO") and len(s) == 24 and s.isalnum() and _mod97_ok(s)
    return IbanCheck(s, valid, s[4:8] if valid else "")
