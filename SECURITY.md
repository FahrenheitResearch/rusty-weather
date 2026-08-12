# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability that could expose
stores, credentials, or service hosts. Use the repository's
[private vulnerability-reporting form](https://github.com/FahrenheitResearch/rusty-weather/security/advisories/new)
with the affected version, reproduction steps, and potential impact. If that
form is unavailable, use a private contact method listed on the Fahrenheit
Research organization profile and identify the message as a Rusty Weather
security report. A public advisory and credit will be coordinated after a fix
is available.

## Deployment boundary

Rusty Weather is safe-by-default for a local single-node deployment:

- the default listener is loopback-only;
- a non-loopback bind is refused without authentication unless an explicit
  unsafe override is supplied;
- the weather store should be mounted read-only;
- API tokens are loaded from environment variables or protected files;
- TLS termination is delegated to a maintained reverse proxy;
- request, response, concurrency, cache, job, and deadline limits are bounded;
- CORS is disabled unless exact origins are configured;
- public errors and health responses omit local paths, commands, and logs.

The example configuration is not a substitute for host network controls,
operating-system updates, secret-file permissions, backups, or upstream proxy
limits.

## Supported versions

Before the first tagged server release, security fixes are applied to the
active service integration line. Beginning with `v0.5.0`, the latest tagged
pre-1.0 minor series receives security fixes; older minor series are unsupported
unless a release advisory explicitly says otherwise. Operators should deploy
the latest patch in the supported series.

| Version | Supported |
| --- | --- |
| `0.5.x` | Yes, once released |
| `< 0.5.0` | No |

This table will be advanced when a newer minor series is released.

## Supply chain

Release candidates must pass dependency advisory and license checks, include a
CycloneDX SBOM and third-party notices, and publish SHA-256 checksums. Vendored
code and static assets retain their own license and attribution files.
Distributed releases must additionally satisfy the multi-node privacy,
signature, quota, recovery, deployment, and packaged-workflow evidence in
[`docs/DISTRIBUTED_RELEASE_GATES.md`](docs/DISTRIBUTED_RELEASE_GATES.md). A
disabled feature, mock transport, or unit-test-only path is not production
evidence.
