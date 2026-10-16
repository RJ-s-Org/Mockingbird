# Security policy

## Supported versions

Security fixes are released for the latest minor version only. Upgrade to the newest
`0.x` (and later `x.y`) release to receive them.

| Version | Supported |
| --- | --- |
| latest minor (currently 0.1.x) | yes |
| anything older | no |

## Reporting a vulnerability

Please do **not** open a public issue, discussion or pull request for a vulnerability.

Report it privately through GitHub Security Advisories: open the repository's
**Security** tab and choose **Report a vulnerability**. Include the affected version,
the configuration involved, steps to reproduce and the impact you expect.

- We acknowledge every report within **3 business days**.
- We aim to ship a fix, and publish an advisory crediting you (unless you prefer
  otherwise), within a **90-day disclosure window** from the report. If a fix needs
  longer, we agree a new date with you before the window ends.
- Please keep the details private until the advisory is published or the 90 days have
  passed.

## Scope

This policy covers the core repository only: the `mockingbird` binary, the crates in
`crates/`, the embedded console, the container images and the deployment files
(`deploy/`) published from it. Third-party dependencies should be reported upstream;
tell us too if Mockingbird is affected. Extensions and binaries built by others on top
of `mb-ext` are out of scope.

## Verifying releases

Release container images are signed keylessly with [cosign](https://github.com/sigstore/cosign)
from the release workflow, and carry a CycloneDX SBOM and SLSA build provenance as
attestations. Verify an image before running it (`<owner>` is the GitHub owner of the
repository the image comes from, `rjs-git` for the official images, and `<v>` the
release version):

```sh
cosign verify ghcr.io/<owner>/mockingbird:<v> \
  --certificate-identity-regexp 'https://github.com/<owner>/mockingbird/.github/workflows/release.yml@refs/tags/v.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Verify the build provenance of an image or a downloaded release binary with the GitHub CLI:

```sh
gh attestation verify oci://ghcr.io/<owner>/mockingbird:<v> --owner <owner>
gh attestation verify <downloaded-release-file> --owner <owner>
```

A verification failure means the artifact was not built by this repository's release
workflow from a `v*` tag: do not run it, and report it as above.
