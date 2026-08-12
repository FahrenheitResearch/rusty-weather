# Cloudflare R2 hot-object gateway

This Worker is the deployable R2/CDN tier for Rusty Weather Community Cache.
It exposes only the closed key grammar used by Rusty Weather:

- `/<bucket-path>/v1/objects/<object-sha256>`
- `/<bucket-path>/v2/manifests/<manifest-sha256>.json`
- `/<bucket-path>/v2/requests/<request-sha256>.json`

Legacy `v1/manifests/<request-sha256>.json` remains readable during migration,
but new writes use v2. Objects and v2 signed-manifest blobs are immutable and
content-addressed. The small v2 request pointer is the sole replaceable key;
its strict body names the request hash and an immutable manifest hash. This
allows a renewed origin signature after the previous manifest expires without
changing canonical request or object identity.

`GET` is public because BowEcho verifies every origin-signed manifest, exact
request identity, size, expiry, object SHA-256, and decoded payload before use.
`PUT` requires the Worker secret `WRITE_BEARER_TOKEN`, exact
`application/octet-stream`, and a bounded nonzero `Content-Length`. Immutable
keys additionally require `If-None-Match: *`; Rusty Weather treats a `412` as
success only after fetching and byte-comparing the existing value. A request
pointer rejects conditional-create headers, unknown JSON fields, mismatched
request hashes, and malformed manifest hashes, then atomically replaces only
that pointer and purges its edge entry. There is no list, delete,
arbitrary-path, redirect, or upload-form endpoint.

Objects and v2 signed manifests are cached at the Cloudflare edge for one year
because their keys are content addresses. Request pointers use a 30-second
must-revalidate TTL. Every client hashes the manifest blob and still performs
the mandatory origin-signature, canonical-request, and expiry checks.

## Deploy

1. Create a dedicated R2 bucket and replace `bucket_name` in `wrangler.jsonc`.
2. Choose the public path component in `BUCKET_PATH`. Configure Rusty Weather's
   R2 `bucket` and BowEcho's public R2 base so the resulting URLs match it.
3. Generate a random bearer token of at least 32 bytes and install it without
   putting it in source or `wrangler.jsonc`:

       npx wrangler secret put WRITE_BEARER_TOKEN

4. Run `npm ci && npm run audit && npm run check && npm run sbom && npm run licenses`.
   Review the generated CycloneDX SBOM, locked Node build-tool license bundle,
   Worker/R2 account limits, and the Wrangler dry-run bundle before running
   `npm run deploy`. Set `workers_dev` only in an explicitly reviewed test
   environment; production should use a TLS custom domain. The release archive
   is a deployable Worker bundle plus its exact `wrangler.jsonc`; it never
   contains `node_modules`.
5. Put the same token in the Rusty Weather R2 gateway token file (mode `0600`).
   BowEcho receives only the public GET base URL, never this token.

The hard ceilings are 1 KiB per request pointer, 256 KiB per signed manifest,
and 64 MiB per encoded object. Environment values may lower the latter two but
cannot raise them. They must not exceed the matching Rusty Weather quotas.
