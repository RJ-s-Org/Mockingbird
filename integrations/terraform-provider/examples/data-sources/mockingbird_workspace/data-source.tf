data "mockingbird_workspace" "current" {}

output "my_role" {
  value = data.mockingbird_workspace.current.role
}
