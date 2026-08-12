# R2 gateway release bundle

This directory is the exact Worker output produced by the tagged Rusty Weather
source and pinned Wrangler version. Verify the archive's published SHA-256,
GitHub build-provenance attestation, CycloneDX document, and license inventory
before deployment.

`index.js` is already bundled. After replacing the placeholder R2 bucket name
in `wrangler.jsonc` through a reviewed deployment configuration, deploy without
rebundling it:

```sh
npx --yes --package wrangler@4.121.0 wrangler deploy \
  --no-bundle --config wrangler.jsonc
```

The command downloads only the exact Wrangler version used to create the
bundle. In a controlled deployment environment, prefer a preinstalled copy
verified against `package-lock.json`. Supply `WRITE_BEARER_TOKEN` as a
secret and never place it in the archive or configuration. The signed Rusty
Weather object manifests remain the authority for object identity; this Worker
only stores and returns bounded immutable bytes.
