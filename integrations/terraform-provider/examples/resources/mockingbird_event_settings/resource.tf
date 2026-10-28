resource "mockingbird_event_settings" "orders_events" {
  mock   = mockingbird_mock.orders_events.name
  broker = "kafka" # a broker named in the server's mockingbird.yaml
}
