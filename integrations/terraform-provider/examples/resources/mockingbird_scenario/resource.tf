# Selected per call with the header `X-Mockingbird-Scenario: card-declined`.
resource "mockingbird_scenario" "card_declined" {
  mock         = mockingbird_mock.stripe.name
  name         = "card-declined"
  instructions = "The card is declined for insufficient funds."
  status       = 402
  operations   = ["PostPaymentIntents", "PostPaymentIntentsIntentConfirm"]
  set = jsonencode({
    error = { code = "card_declined", decline_code = "insufficient_funds" }
  })
}

resource "mockingbird_scenario" "slow" {
  mock       = mockingbird_mock.stripe.name
  name       = "slow"
  latency_ms = 3000
}
