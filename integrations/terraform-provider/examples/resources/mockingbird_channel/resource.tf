# Publish a generated order event every 30 seconds instead of on demand.
resource "mockingbird_channel" "order_created" {
  mock     = mockingbird_mock.orders_events.name
  channel  = "orderCreated"
  enabled  = true
  triggers = jsonencode([{ schedule = { every_ms = 30000 } }])
}
