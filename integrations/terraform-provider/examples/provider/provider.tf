terraform {
  required_providers {
    mockingbird = {
      source = "RJ-s-Org/mockingbird"
    }
  }
}

# Every setting can come from the environment instead:
# MB_ADMIN_URL, MB_ADMIN_TOKEN, MB_WORKSPACE and MB_DATA_PLANE_URL.
provider "mockingbird" {
  endpoint       = "http://127.0.0.1:9090"
  workspace      = "default"
  data_plane_url = "http://127.0.0.1:8080"
  # token = var.mockingbird_token # an API key (mbk_…) or OIDC access token
}
