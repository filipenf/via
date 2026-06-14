# Security Policy

## Supported Versions

via is a small experimental project. Only the latest tagged release is supported
for security fixes.

## Reporting a Vulnerability

If you discover a security issue (e.g. unsafe PTY handling, command injection
via agent-controlled paths, socket permission problems, or anything that could
lead to arbitrary code execution or data exfiltration on the host), please
report it privately.

Preferred: use GitHub's private security advisory feature (Security >
Advisories > "Report a vulnerability") for this repository.

You may also email the maintainer at the address listed in the git commit
history / Cargo.toml authors if the above is not available.

Please include:

- A clear description of the issue and affected versions.
- Steps to reproduce (sanitized logs or minimal reproduction case).
- Potential impact and suggested fix if you have one.

We will acknowledge receipt within a few days and work with you on a coordinated
disclosure and fix. Thank you for helping keep via users safe.
