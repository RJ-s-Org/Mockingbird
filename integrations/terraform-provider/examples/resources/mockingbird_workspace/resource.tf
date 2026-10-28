resource "mockingbird_workspace" "staging" {
  name         = "staging"
  display_name = "Staging"
  # Also delete the workspace's mocks and LLM profiles on destroy.
  force_destroy = true
}
