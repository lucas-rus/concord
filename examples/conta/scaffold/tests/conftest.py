import json
from pathlib import Path

import pytest

DATA = Path(__file__).parent / "data"


@pytest.fixture
def invoice_data():
    return json.loads((DATA / "factura_simpla.json").read_text())
