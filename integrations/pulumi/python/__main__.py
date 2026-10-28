"""A mocked staging environment: Stripe and GitHub mocks the staging app points at."""

import pulumi
import pulumi_mockingbird as mockingbird

# Contracts from the examples catalog (`mockingbird examples init stripe`).
config = pulumi.Config()
stripe_contract = config.get("stripeContract") or "../../../examples/apis/stripe/stripe.openapi.yaml"
github_contract = config.get("githubContract") or "../../../examples/apis/github-rest/github-rest.openapi.yaml"

# The provider reads MB_ADMIN_URL, MB_ADMIN_TOKEN and MB_DATA_PLANE_URL.
stripe = mockingbird.Mock(
    "stripe",
    name="stripe",
    route_prefix="/stripe",
    persona="Stripe in test mode for Juniper & Co, a plant shop in Portland.",
    contract={"file": stripe_contract},
)

mockingbird.Scenario(
    "card-declined",
    mock=stripe.name,
    name="card-declined",
    instructions="The card is declined for insufficient funds.",
    status=402,
    operations=["PostPaymentIntents", "PostPaymentIntentsIntentConfirm"],
)

github = mockingbird.Mock(
    "github",
    name="github",
    route_prefix="/github",
    persona="GitHub for octo-org, a 40-person company with a dozen repos.",
    contract={"file": github_contract},
)

# What the staging app's environment points at.
pulumi.export("stripe_api_base", stripe.url)
pulumi.export("github_api_url", github.url)
pulumi.export("stripe_contract_sha256", stripe.contract_sha256)
