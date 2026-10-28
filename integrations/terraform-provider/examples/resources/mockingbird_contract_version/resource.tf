resource "mockingbird_mock" "billing" {
  name = "billing"
}

# Each change of the file uploads a new, immutable version and activates it.
resource "mockingbird_contract_version" "billing" {
  mock     = mockingbird_mock.billing.name
  file     = "${path.module}/contracts/billing.yaml"
  activate = true
}
