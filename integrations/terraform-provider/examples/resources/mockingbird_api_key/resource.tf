# A key the staging app sends to mocks with data_plane_auth.mode = "api_key".
resource "mockingbird_api_key" "app" {
  name   = "staging-app"
  role   = "viewer"
  scopes = ["data-plane"]
}

# Hand the token to the app's secret store rather than printing it.
output "app_key" {
  value     = mockingbird_api_key.app.token
  sensitive = true
}
