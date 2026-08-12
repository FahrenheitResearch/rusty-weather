#!/bin/sh
set -eu

# This script runs only in Dockerfile.rw-lab's throw-away build tree. It does
# not alter the checkout and it does not disable TLS verification. The lab
# binary trusts one generated isolated-lab CA in addition to preserving all
# production DNS, redirect, SNI, hostname, timeout, and address checks.
# `&` is special in a sed replacement; the backslash emits the Rust borrow.
replacement='ureq::tls::RootCerts::new_with_certs(\&[ureq::tls::Certificate::from_pem(include_bytes!("/lab/ca.crt")).expect("valid distributed-lab CA")])'

for source in \
    crates/rw-server/src/community/network.rs \
    crates/rw-server/src/community_relay_provider.rs \
    crates/rw-server/src/federation.rs \
    crates/rw-federation-proxy/src/lib.rs
do
    occurrences=$(grep -F -c 'ureq::tls::RootCerts::WebPki' "${source}")
    if [ "${occurrences}" -ne 1 ]; then
        echo "expected exactly one audited WebPki trust seam in ${source}" >&2
        exit 1
    fi
    sed -i "s@ureq::tls::RootCerts::WebPki@${replacement}@g" "${source}"
    if grep -F -q 'ureq::tls::RootCerts::WebPki' "${source}"; then
        echo "not every WebPki trust seam was replaced in ${source}" >&2
        exit 1
    fi
done

# The direct-IP policy remains compiled exactly as production wrote it.
grep -F -q 'PublicInternetOnly' crates/rw-server/src/community/network.rs
grep -F -q 'addresses.iter().any(|address| !is_global_ip(address.ip()))' \
    crates/rw-federation-proxy/src/lib.rs
grep -F -q 'DirectCandidateForbidden' crates/rw-community-protocol/src/lib.rs
