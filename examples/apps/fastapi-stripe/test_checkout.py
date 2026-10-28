import re
import time

import httpx


def test_checkout_creates_a_payment_intent(app_url):
    r = httpx.post(f"{app_url}/checkout", json={"plant": "Fiddle leaf fig", "amount": 4500})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["id"].startswith("pi_")
    # Offline, the faker follows the contract's pattern; with an LLM the
    # secret also repeats the intent's id, as Stripe's does.
    assert re.fullmatch(r"pi_\w{24}_secret_\w{24}", body["client_secret"])


def test_signed_webhook_reaches_the_app(app_url):
    r = httpx.post(f"{app_url}/checkout", json={"plant": "Monstera", "amount": 3900})
    assert r.status_code == 200, r.text
    intent = r.json()["id"]
    # The mock signs payment_intent.created with Stripe-Signature; the app
    # only records it if stripe.Webhook.construct_event accepts it.
    deadline = time.time() + 15
    while time.time() < deadline:
        events = httpx.get(f"{app_url}/events").json()
        if any(e["type"] == "payment_intent.created" for e in events):
            return
        time.sleep(0.2)
    raise AssertionError(f"no verified payment_intent.created event for {intent}: {events}")


def test_card_declined_becomes_a_402(app_url):
    r = httpx.post(
        f"{app_url}/checkout",
        json={"plant": "Snake plant", "amount": 2500},
        headers={"x-scenario": "card-declined"},
    )
    assert r.status_code == 402, r.text
    assert r.json()["detail"]["code"] == "card_declined"
