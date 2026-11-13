import dataclasses
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest

from rustain_harbor.agent import PROVIDERS
from rustain_harbor.preflight import check

GOOD_KEY = "sk-test-accepted"
MODEL = "deepseek/deepseek-v4-flash"


class KeyCheckingHandler(BaseHTTPRequestHandler):
    """Answers like a provider's model list: 200 for the right bearer key, else 401."""

    def do_GET(self):
        accepted = self.headers.get("Authorization") == f"Bearer {GOOD_KEY}"
        self.send_response(200 if accepted else 401)
        self.end_headers()
        self.wfile.write(b"{}")

    def log_message(self, *args):
        pass


@pytest.fixture
def provider_endpoint(monkeypatch):
    server = HTTPServer(("127.0.0.1", 0), KeyCheckingHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/models"
    monkeypatch.setitem(
        PROVIDERS, "deepseek", dataclasses.replace(PROVIDERS["deepseek"], key_check_url=url)
    )
    monkeypatch.setenv("no_proxy", "127.0.0.1")
    yield url
    server.shutdown()
    server.server_close()


def test_accepted_key_passes(provider_endpoint):
    assert check(MODEL, {"DEEPSEEK_API_KEY": GOOD_KEY}) is None


def test_rejected_key_names_the_variable_without_leaking_it(provider_endpoint):
    problem = check(MODEL, {"DEEPSEEK_API_KEY": "sk-test-rejected"})

    assert problem == "deepseek rejected DEEPSEEK_API_KEY (HTTP 401)"


def test_missing_key_is_reported_without_a_request():
    assert check(MODEL, {}) == "DEEPSEEK_API_KEY is not set"


def test_unreachable_provider_is_reported(monkeypatch):
    monkeypatch.setitem(
        PROVIDERS,
        "deepseek",
        dataclasses.replace(
            PROVIDERS["deepseek"], key_check_url="http://127.0.0.1:1/models"
        ),
    )
    monkeypatch.setenv("no_proxy", "127.0.0.1")

    problem = check(MODEL, {"DEEPSEEK_API_KEY": GOOD_KEY})

    assert problem is not None and problem.startswith("cannot reach http://127.0.0.1:1/models")


def test_provider_without_a_key_check_only_needs_the_key_set():
    assert check("anthropic/claude-sonnet-4-5", {"ANTHROPIC_API_KEY": "any"}) is None
