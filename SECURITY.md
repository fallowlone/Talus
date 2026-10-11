# Security policy

Talus handles database credentials and runs SQL against your servers, so security reports are
taken seriously.

## Reporting a vulnerability

Please **do not open a public issue**. Use GitHub's private vulnerability reporting instead:
on the repository page, open **Security → Report a vulnerability**. Include the steps to
reproduce, the affected version and the impact you expect.

You can expect an acknowledgement within a week. Fixes are released as soon as they are ready and
credited to you unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes.

## Scope

In scope: credential handling (Keychain, tunnels), SQL generation (designer, sync, modelling),
import/export, and anything that lets a database or file influence what Talus executes.
Out of scope: vulnerabilities in the database servers themselves.
