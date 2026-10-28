import json
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from mockingbird_testcontainers import MockingbirdContainer

FIXTURES = Path(__file__).parent.parent / "fixtures"


@pytest.fixture(scope="module")
def mocks():
    with MockingbirdContainer().with_config_dir(FIXTURES) as container:
        yield container


def post(url, body, headers=None):
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json", **(headers or {})},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as res:
            return res.status, json.load(res)
    except urllib.error.HTTPError as err:
        return err.code, json.load(err)


def test_a_charge_is_created_and_recorded(mocks):
    status, charge = post(f"{mocks.base_url('payments')}/v1/charges", {"amount": 1999, "currency": "eur"})
    assert status == 201
    assert isinstance(charge["id"], str)
    log = mocks.requests("payments")
    assert any(r["method"] == "POST" and r["request"]["body"]["amount"] == 1999 for r in log)


def test_a_scenario_declines_the_card(mocks):
    status, _ = post(
        f"{mocks.base_url('payments')}/v1/charges",
        {"amount": 1999, "currency": "eur"},
        {"x-mockingbird-scenario": "card-declined"},
    )
    assert status == 402
