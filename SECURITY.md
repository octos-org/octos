# Security Policy

The Octos maintainers take security reports seriously. Please do not disclose a
suspected vulnerability in a public issue, pull request, discussion, commit
message, or log attachment.

Repository administrators should keep GitHub private vulnerability reporting
enabled. The contact-request fallback below exists for periods when that
repository setting is unavailable.

## Supported versions

Security fixes are prepared for the latest published release and the current
`main` branch. Older releases may be asked to upgrade rather than receive a
backport. Pre-release builds and unreleased commits are evaluated case by case.

| Version | Security updates |
| --- | --- |
| Latest published release | Supported |
| Current `main` | Evaluated for the next release |
| Older releases | Not normally supported |

## Reporting a vulnerability

1. On the repository's **Security** page, use **Report a vulnerability** if that
   private reporting option is available.
2. If it is unavailable, open a
   [security contact request](https://github.com/octos-org/octos/issues/new?template=security_contact.yml).
   Include only contact information and a one-line, non-sensitive category.
   **Do not include exploit steps, logs, secrets, affected paths, or technical
   details.** A maintainer will establish a private channel before details are
   exchanged.

Include the following only in the private report:

- affected version, commit, and platform;
- impact and the conditions required to reproduce it;
- minimal reproduction or proof of concept;
- whether the issue is known to be actively exploited;
- suggested remediation, if available; and
- your preferred credit and disclosure timeline.

If a report affects the octoscode terminal client rather than this server
kernel, say so; the maintainers will coordinate with the
[octoscode repository](https://github.com/octos-org/octoscode).

## What to expect

Maintainers will make a best effort to acknowledge a complete report within
five business days. The project will validate the finding, agree on a
coordinated disclosure plan, prepare tests and a fix, and publish an advisory
when users need to act. Timing depends on severity and release complexity; do
not publish details before the agreed disclosure date.

The reporter should receive status updates when the issue is confirmed, when a
fix is ready for verification, and when disclosure is scheduled. Duplicate,
non-security, or out-of-scope reports may be closed with an explanation.

## Scope

In scope:

- the Octos server kernel, the `octos` CLI, and official release artifacts,
  including the serve, bus, and agent runtime surfaces;
- credential or token exposure caused by Octos;
- command execution, path traversal, permission-boundary, update, archive, or
  dependency-integrity vulnerabilities; and
- protocol handling in the server that creates a security impact.

Generally out of scope:

- vulnerabilities only in the separate octoscode terminal client, a fork
  repository, or a third-party provider;
- unsupported, end-of-life versions;
- social engineering, denial of service requiring unrealistic resources, or
  reports without a plausible security impact; and
- findings produced solely by an automated scanner without validation.

## Safe harbor

Good-faith research that follows this policy, avoids privacy violations and
service disruption, accesses only data you own or are authorized to use, and
allows reasonable time for remediation will not be intentionally pursued by
the project. This statement does not authorize testing against third-party
systems and cannot bind third parties.
