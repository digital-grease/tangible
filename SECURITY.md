# Security Policy

## Reporting a vulnerability

Report suspected vulnerabilities privately, through one of these channels:

1. **GitHub private vulnerability reporting** (preferred):
   https://github.com/digital-grease/tangible/security/advisories/new
   opens a private advisory that only the maintainer can see.
2. **Email**: dg@digitalgrease.net, for reporters without a GitHub account.
   To encrypt the report, ask for a key in a short first message.

Do not open a public issue, pull request or discussion about a suspected
vulnerability.

Please include:

- the affected version, tag or commit
- the impact: what an attacker can do, and from where
- steps to reproduce
- logs or fixtures, with secrets removed
- a suggested fix, if you have one

Treat these as security-sensitive: path traversal, command construction,
SSRF, authentication of operators or workers, device access, artifact
corruption, burn-job duplication, and anything that could erase or overwrite a
disc nobody asked to have written.

## What happens next

Tangible is maintained by one person, so these are aims rather than
guarantees:

- an acknowledgement within a week
- an assessment, and a plan or a reason for declining, within thirty days
- a fix released, and a GitHub security advisory published crediting the
  reporter unless they ask otherwise, once users can update

Please allow a fix to be released before disclosing publicly. If no fix has
been released ninety days after your report, you are free to disclose,
unless we have agreed a different date together.

## Supported versions

Tangible is pre-release. Only the latest release receives security fixes.

## Verifying what you run

Release images are signed and attested; see "Verifying a release" in
`deploy/README.md`.
