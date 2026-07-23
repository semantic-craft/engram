# Security Policy

## Reporting a vulnerability

Please **do not open a public GitHub issue** for security vulnerabilities.

Report security issues by opening a [private security advisory](https://github.com/semantic-craft/engram/security/advisories/new)
on GitHub. You will receive a response within 7 days. If the issue is confirmed
we will aim to release a patch within 30 days and credit you in the changelog
(unless you prefer to remain anonymous).

## Threat model

engram is a **single-user, homelab tool**. The following describes what
the project is and is not designed to defend against.

### In scope

- **Local data confidentiality.** Wiki files and the SQLite database live
  under a single data directory controlled by the operating-system user who
  runs the server. We rely on filesystem permissions; no additional
  encryption at rest is provided in v1.

- **Network exposure when binding to non-loopback addresses.** If you run
  `engram serve --bind 0.0.0.0:…` you are exposing the MCP and admin
  routes to your local network. Protect this with:
  - `ENGRAM_AUTH_TOKEN` / `engram generate-auth-token` (bearer token
    checked on every request).
  - Firewall rules or a reverse proxy with TLS.

  The server logs a loud warning if it detects a non-loopback bind without a
  configured auth token.

- **Host-header DNS rebinding.** The HTTP server enforces an
  `ENGRAM_ALLOWED_HOSTS` allowlist (defaulting to `127.0.0.1` and
  `localhost`). Requests with a `Host` header not in the list are rejected
  with 403.

- **Request body size.** Inbound HTTP bodies are capped at 10 MB to prevent
  trivial memory exhaustion.

- **Per-project isolation.** Wiki files and SQLite rows are namespaced by
  `(workspace_id, project_id)`. A purge operation for project A cannot
  delete files that also belong to project B.

### Out of scope for v1

- **Multi-tenant authentication and authorisation.** There is one bearer
  token (or none). There are no per-user roles or per-project ACLs.
- **Encryption at rest.** The data directory is a plain filesystem tree.
- **Remote sync security.** If you push the wiki git repository to a remote,
  securing that channel is your responsibility (SSH keys, GitHub access
  controls, etc.).
- **MCP tool-call injection via agent output.** The privacy strip
  (`Sanitizer`) removes obvious credential patterns from hook payloads, but
  it is not a comprehensive injection fence.
- **Denial of service.** The server is not hardened against a malicious local
  actor hammering it with requests.

## Supported versions

Only the latest release receives security fixes. We do not backport to older
minor versions.
