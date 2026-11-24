"""Redaction guard for J7 evidence (AC5): nothing is saved if a credential value or a
secret-bearing field would be archived.

``FORBIDDEN_FRAGMENTS`` are the fixture's own (fake) credentials and the field/URL
shapes that carry them. The guard is a fail-closed allowlist check on the text about to
be written, run BEFORE any evidence file is created; the pytest controls feed it
deliberately injected synthetic canaries only.
"""

from __future__ import annotations

from pathlib import Path

# Shapes that carry secrets: request URL with the token segment, auth headers/keys.
SECRET_FIELDS = ("/bot", "x-api-key", "authorization", "bearer ", "proof", "api_key", "secret", "private key")


class RedactionError(AssertionError):
    pass


def violations(text: str, credential_values: tuple[str, ...]) -> list[str]:
    lowered = text.lower()
    found = [f"credential value ({len(v)} chars)" for v in credential_values if v and v in text]
    found += [f"secret-bearing field {f!r}" for f in SECRET_FIELDS if f in lowered]
    return found


def write_checked(path: Path, text: str, credential_values: tuple[str, ...]) -> None:
    """Write ``text`` to ``path`` only if it carries no credential or secret field."""
    bad = violations(text, credential_values)
    if bad:
        raise RedactionError(f"refusing to archive {path.name}: {', '.join(bad)}")
    path.write_text(text)
