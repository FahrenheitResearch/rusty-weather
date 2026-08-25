# Windows service with WinSW

WinSW is not bundled. Download a trusted WinSW release from its official
repository, verify its published checksum, rename it to
`rusty-weather-service.exe`, and place it beside:

- `rusty-weather-service.xml`
- `rw-server.exe`
- `rusty-weather.toml`

Create `data\store`, `data\artifacts`, `logs`, and `secrets`. Put one random
API token of at least 32 bytes per line in `secrets\api-tokens.txt`. If a
distributed feature is explicitly enabled, also create only the corresponding
`data\community-cache`, `data\federation`, or
`data\generation-replication` control directory. Server-owned satellite ingest
instead uses a separate `data\satellite-staging` raw-download directory. These
are service state, not
arbitrary upload directories, and `data\store` remains read-only to the API in
normal scheduler-origin mode.

The template runs as the built-in NetworkService identity. From an elevated
PowerShell prompt in the install directory, grant that identity read/execute
access to the application and store, and modify access only to artifacts and
logs. Then replace the token DACL with entries for NetworkService and SYSTEM:

    $serviceSid = '*S-1-5-20' # NT AUTHORITY\NETWORK SERVICE
    $systemSid = '*S-1-5-18'  # NT AUTHORITY\SYSTEM
    icacls . /grant:r "${serviceSid}:(OI)(CI)(RX)"
    icacls .\data\artifacts /grant:r "${serviceSid}:(OI)(CI)(M)"
    icacls .\logs /grant:r "${serviceSid}:(OI)(CI)(M)"
    icacls .\secrets\api-tokens.txt /inheritance:r
    icacls .\secrets\api-tokens.txt /grant:r "${serviceSid}:(R)" "${systemSid}:(F)"
    icacls .\secrets\api-tokens.txt

For each enabled distributed feature, grant `NetworkService` modify access to
that feature's control directory only. Replication is the one advanced mode
that also requires write access to `data\store`; grant it only after the
capacity/security gates and publication-source policy have been reviewed:

    icacls .\data\community-cache /grant:r "${serviceSid}:(OI)(CI)(M)"
    icacls .\data\federation /grant:r "${serviceSid}:(OI)(CI)(M)"
    icacls .\data\generation-replication /grant:r "${serviceSid}:(OI)(CI)(M)"
    # Advanced replication-only/union origin:
    # icacls .\data\store /grant:r "${serviceSid}:(OI)(CI)(M)"

When `satellite_ingest.enabled = true`, grant modify access only to its staging
root and the shared store; client requests still cannot start ingestion:

    icacls .\data\satellite-staging /grant:r "${serviceSid}:(OI)(CI)(M)"
    icacls .\data\store /grant:r "${serviceSid}:(OI)(CI)(M)"

When `mrms_ingest.enabled = true`, no separate staging directory is used. Grant
the same narrowly scoped modify access to `data\store`, retain API tokens, and
monitor the authenticated MRMS status route. One server follower supplies the
stored frames shared by every client; client requests do not each download or
decode MRMS.

Keep Community, relay, federation, operations, and generation signing keys plus
provider tokens as separate regular files under `secrets`. Remove inherited ACLs and
grant read only to `NetworkService` and full control to `SYSTEM`, using the
same pattern as `api-tokens.txt`; never put secret text in the XML or TOML.

The token ACL inspection command must not show access for Users, Authenticated
Users, or Everyone. Adapt the service SID if the XML is changed to a dedicated
account.
Rust's portable file APIs cannot evaluate Windows DACLs, so rw-server doctor
checks token structure and readability but does **not** enforce this Windows ACL
policy. The installer/operator must review it with icacls or Get-Acl.

Install and validate:

    .\rw-server.exe --config .\rusty-weather.toml doctor
    .\rusty-weather-service.exe install
    .\rusty-weather-service.exe start
    .\rusty-weather-service.exe status
    .\rw-server.exe --config .\rusty-weather.toml healthcheck

The service healthcheck succeeds for both `ready` and `degraded` HTTP 200
responses. Degraded means an optional MRMS or NEXRAD Level II feed is warming, stale,
or backing off while core model, satellite, query, and operations traffic is
still usable. Monitor the JSON body and authenticated subsystem status routes
separately; do not restart the Windows service solely for degraded upstream
data.

For upgrades, stop the service, back up the configuration and store manifests,
replace only the versioned application files, run `doctor`, then start it. Keep
the previous signed package available for rollback. Do not put the service on a
public interface without API tokens and a TLS reverse proxy.

## Optional scheduler service

Use a second verified copy of WinSW named
`rusty-weather-scheduler-service.exe` beside
`rusty-weather-scheduler-service.xml`, `rw-scheduler.exe`, and a production copy
of `rusty-weather-scheduler.toml`. In that TOML, use three absolute, distinct,
non-nested Windows roots for the shared store, scheduler-only cache, and
scheduler-only state. Keep retention disabled and dry-run until its plan has
been reviewed.

The scheduler template runs as `LocalService`, separate from the API template's
`NetworkService`. Create `data\ingest-cache`, `data\scheduler-state`, and
`logs\scheduler`, then grant only the scheduler identity modify access to its
state/cache and to the shared store:

    $schedulerSid = '*S-1-5-19' # NT AUTHORITY\LOCAL SERVICE
    icacls . /grant:r "${schedulerSid}:(OI)(CI)(RX)"
    icacls .\data\store /grant:r "${schedulerSid}:(OI)(CI)(M)"
    icacls .\data\ingest-cache /grant:r "${schedulerSid}:(OI)(CI)(M)"
    icacls .\data\scheduler-state /grant:r "${schedulerSid}:(OI)(CI)(M)"
    icacls .\logs\scheduler /grant:r "${schedulerSid}:(OI)(CI)(M)"

Leave the API's `NetworkService` identity read-only on `data\store`. Validate
and install the writer independently:

    .\rw-scheduler.exe --config .\rusty-weather-scheduler.toml plan
    .\rusty-weather-scheduler-service.exe install
    .\rusty-weather-scheduler-service.exe start
    .\rusty-weather-scheduler-service.exe status
    .\rw-scheduler.exe --config .\rusty-weather-scheduler.toml status

The WinSW template allows 120 seconds for shutdown. Durable running jobs are
recovered on restart; do not run a second scheduler against the same state and
store roots.
