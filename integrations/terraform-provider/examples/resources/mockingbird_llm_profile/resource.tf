# The key stays on the Mockingbird server: secret_ref names the environment
# variable it reads, so no secret reaches the Terraform state.
resource "mockingbird_llm_profile" "default" {
  name        = "openrouter"
  base_url    = "https://openrouter.ai/api/v1"
  model       = "deepseek/deepseek-v4.1-flash-1017"
  secret_ref  = "env:OPENROUTER_API_KEY"
  json_schema = true
  temperature = 0.4
  headers = {
    "x-title" = "Mockingbird staging"
  }
  extra_body = jsonencode({ provider = { sort = "price" } })
}
