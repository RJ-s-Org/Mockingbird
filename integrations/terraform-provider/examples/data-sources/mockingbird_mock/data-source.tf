# Read a mock declared in the server's config file.
data "mockingbird_mock" "payments" {
  name = "payments"
}

output "payments_operations" {
  value = data.mockingbird_mock.payments.operations
}
