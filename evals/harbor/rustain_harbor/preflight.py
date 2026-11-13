"""Checks the provider key before any task container starts.

    python -m rustain_harbor.preflight deepseek/deepseek-v4-flash

Without it a rejected key fails every trial on its first provider call, after
the task images have been pulled.
"""

from __future__ import annotations

import os
import sys
import urllib.error
import urllib.request
from collections.abc import Mapping

from rustain_harbor.agent import PROVIDERS


def check(model: str, env: Mapping[str, str]) -> str | None:
    """Why a run of `model` cannot authenticate, or None when nothing is wrong.

    Providers without a `key_check_url` are only checked for a key being set.
    """
    provider_id = model.partition("/")[0]
    provider = PROVIDERS.get(provider_id)
    if provider is None:
        return (
            f"provider '{provider_id}' is not supported by this adapter; "
            f"supported: {', '.join(sorted(PROVIDERS))}"
        )
    key = env.get(provider.api_key_env)
    if not key:
        return f"{provider.api_key_env} is not set"
    if provider.key_check_url is None:
        return None

    request = urllib.request.Request(
        provider.key_check_url, headers={"Authorization": f"Bearer {key}"}
    )
    try:
        with urllib.request.urlopen(request, timeout=20):
            return None
    except urllib.error.HTTPError as error:
        if error.code in (401, 403):
            return f"{provider_id} rejected {provider.api_key_env} (HTTP {error.code})"
        # Rate limits and server errors say nothing about the key.
        return None
    except OSError as error:
        return f"cannot reach {provider.key_check_url}: {error}"


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: python -m rustain_harbor.preflight <provider>/<model>", file=sys.stderr)
        return 2
    problem = check(sys.argv[1], os.environ)
    if problem:
        print(f"preflight: {problem}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
