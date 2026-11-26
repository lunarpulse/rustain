"""The one dated price table the harness prices runs with.

The table is a file in this package (`PRICE_TABLE`). It is read from disk and
nothing is fetched: Harbor's own LiteLLM price map plays no part in the cost.
The adapter prices each trial from it, renders the same rates as rustain's
`[pricing]` config so `ask --max-cost` agrees with the harness, and the
eval-4 run header records its hash.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path

from rustain_harbor.usage import Prices

PRICE_TABLE = Path(__file__).with_name("prices-2026-11-26.json")
_RATES = ("input", "output", "cache_read", "cache_creation")


@dataclass(frozen=True)
class PriceTable:
    id: str
    date: str
    source: str
    sha256: str
    # `<provider>/<model>` -> USD per million tokens, keyed by `_RATES`.
    per_million: dict[str, dict[str, float]]

    def prices(self, provider_id: str, model: str) -> Prices | None:
        """Per-token prices of `<provider_id>/<model>`, or None when unlisted."""
        rates = self.per_million.get(f"{provider_id}/{model}")
        if rates is None:
            return None
        return Prices(**{name: rates[name] / 1_000_000 for name in _RATES})

    def label(self) -> str:
        return f"{self.id} ({self.date}) sha256:{self.sha256}"


def load(path: Path = PRICE_TABLE) -> PriceTable:
    raw = path.read_bytes()
    data = json.loads(raw)
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", str(data.get("date"))):
        raise ValueError(f"{path}: date must be YYYY-MM-DD")
    if data.get("currency") != "USD" or data.get("unit") != "per million tokens":
        raise ValueError(f"{path}: prices must be USD per million tokens")
    per_million: dict[str, dict[str, float]] = {}
    for key, rates in data["models"].items():
        if "/" not in key:
            raise ValueError(f"{path}: model key {key!r} is not <provider>/<model>")
        if set(rates) != set(_RATES):
            raise ValueError(f"{path}: {key} must give exactly {', '.join(_RATES)}")
        for name, value in rates.items():
            if isinstance(value, bool) or not isinstance(value, int | float) or value < 0:
                raise ValueError(f"{path}: {key}.{name} must be a number >= 0")
        per_million[key] = {name: float(rates[name]) for name in _RATES}
    return PriceTable(
        id=data["id"],
        date=data["date"],
        source=data["source"],
        sha256=hashlib.sha256(raw).hexdigest(),
        per_million=per_million,
    )
