"""Juniper & Co checkout: creates Stripe PaymentIntents and records webhook events.

Nothing here knows about Mockingbird. Run it against the real Stripe test API, or
through `mockingbird proxy env` against the mock.
"""

import os

import stripe
from fastapi import FastAPI, Header, HTTPException, Request
from pydantic import BaseModel

stripe.api_key = os.environ.get("STRIPE_API_KEY", "sk_test_123")
# stripe-python verifies TLS against its own CA bundle; honour SSL_CERT_FILE
# (set by `mockingbird proxy env`) when present.
if os.environ.get("SSL_CERT_FILE"):
    stripe.ca_bundle_path = os.environ["SSL_CERT_FILE"]

app = FastAPI(title="Juniper & Co checkout")
events: list[dict] = []


class CheckoutRequest(BaseModel):
    plant: str
    amount: int
    currency: str = "usd"


@app.post("/checkout")
def checkout(req: CheckoutRequest, x_scenario: str | None = Header(default=None)):
    # Lets the test pick a Mockingbird scenario; real Stripe ignores the header.
    headers = {"X-Mockingbird-Scenario": x_scenario} if x_scenario else None
    try:
        intent = stripe.PaymentIntent.create(
            amount=req.amount,
            currency=req.currency,
            description=req.plant,
            headers=headers,
        )
    except stripe.error.CardError as e:
        raise HTTPException(status_code=402, detail={"code": e.code, "message": e.user_message})
    return {"id": intent.id, "client_secret": intent.client_secret, "amount": intent.amount}


@app.post("/webhook")
async def webhook(request: Request, stripe_signature: str = Header()):
    payload = await request.body()
    try:
        event = stripe.Webhook.construct_event(
            payload, stripe_signature, os.environ["STRIPE_WEBHOOK_SECRET"]
        )
    except (ValueError, stripe.error.SignatureVerificationError) as e:
        raise HTTPException(status_code=400, detail=str(e))
    events.append({"id": event["id"], "type": event["type"], "object": event["data"]["object"]["id"]})
    return {"received": True}


@app.get("/events")
def list_events():
    return events
