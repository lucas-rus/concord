"""Romanian fiscal identification code (CUI / CIF) checks."""

from __future__ import annotations

_KEY = "753217532"


def is_valid_cui(cui: str) -> bool:
    """True if `cui` is 2-10 digits with a correct control digit.

    The control digit is computed with the official key 753217532: the
    digits (without the control digit), right-aligned against the key, are
    multiplied pairwise and summed; the sum * 10 mod 11 (10 becomes 0) must
    equal the last digit.
    """
    if not cui.isdigit() or not 2 <= len(cui) <= 10:
        return False
    body, control = cui[:-1], int(cui[-1])
    key = _KEY[-len(body):]
    total = sum(int(a) * int(b) for a, b in zip(body, key))
    expected = total * 10 % 11
    if expected == 10:
        expected = 0
    return expected == control
