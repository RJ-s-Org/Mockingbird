# A Stripe mock from the published OpenAPI document: a new contract version
# is uploaded whenever the document's sha256 changes.
resource "mockingbird_mock" "stripe" {
  name         = "stripe"
  route_prefix = "/stripe"
  persona      = "Stripe's API in test mode, for a European marketplace."

  contract = {
    url = "https://raw.githubusercontent.com/stripe/openapi/master/openapi/spec3.json"
  }
}

# A mock of an internal service from a file in the repository, answering
# only for its own host name, with chaos and a webhook.
resource "mockingbird_mock" "orders" {
  name         = "orders"
  route_prefix = "/"
  hosts        = ["orders.staging.internal"]
  persona      = "The orders service of a grocery delivery app."
  llm_profile  = mockingbird_llm_profile.default.name
  consistency  = "on"

  chaos = {
    error_rate     = 0.02
    latency_jitter = { min_ms = 20, max_ms = 200 }
  }

  limits = {
    rate_limit = { requests_per_second = 20, burst = 40 }
  }

  webhooks = [{
    url    = "https://app.staging.internal/webhooks/orders"
    events = ["createOrder"]
    secret = "env:ORDERS_WEBHOOK_SECRET" # a reference, never the secret
  }]

  contract = {
    file = "${path.module}/contracts/orders.yaml"
  }
}

output "orders_url" {
  # Needs the provider's data_plane_url.
  value = mockingbird_mock.orders.url
}
