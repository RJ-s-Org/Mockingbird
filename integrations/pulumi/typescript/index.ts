import * as pulumi from "@pulumi/pulumi";
import * as mockingbird from "@pulumi/mockingbird";

// Contracts from the examples catalog (`mockingbird examples init stripe`).
const config = new pulumi.Config();
const stripeContract = config.get("stripeContract") ?? "../../../examples/apis/stripe/stripe.openapi.yaml";
const githubContract = config.get("githubContract") ?? "../../../examples/apis/github-rest/github-rest.openapi.yaml";

// The provider reads MB_ADMIN_URL, MB_ADMIN_TOKEN and MB_DATA_PLANE_URL.
const stripe = new mockingbird.Mock("stripe", {
    name: "stripe",
    routePrefix: "/stripe",
    persona: "Stripe in test mode for Juniper & Co, a plant shop in Portland.",
    contract: { file: stripeContract },
});

new mockingbird.Scenario("card-declined", {
    mock: stripe.name,
    name: "card-declined",
    instructions: "The card is declined for insufficient funds.",
    status: 402,
    operations: ["PostPaymentIntents", "PostPaymentIntentsIntentConfirm"],
});

const github = new mockingbird.Mock("github", {
    name: "github",
    routePrefix: "/github",
    persona: "GitHub for octo-org, a 40-person company with a dozen repos.",
    contract: { file: githubContract },
});

// What the staging app's environment points at.
export const stripeApiBase = stripe.url;
export const githubApiUrl = github.url;
export const stripeContractSha256 = stripe.contractSha256;
