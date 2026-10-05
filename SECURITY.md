# Security Policy

## Reporting a vulnerability

Report suspected vulnerabilities privately, through GitHub's private
vulnerability reporting:

https://github.com/digital-grease/tangible/security/advisories/new

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

Please allow a fix to be released before disclosing publicly. If a report
goes unanswered for thirty days, you are free to disclose.

## Supported versions

Tangible is pre-release. Only the latest release receives security fixes.

## Verifying what you run

Release images are signed and attested; see "Verifying a release" in
`deploy/README.md`.
