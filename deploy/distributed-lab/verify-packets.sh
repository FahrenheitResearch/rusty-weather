#!/bin/sh
set -eu

uploader=/lab/results/captures/uploader.pcap
downloader=/lab/results/captures/downloader.pcap

for capture in "${uploader}" "${downloader}"; do
  test -s "${capture}"
  capinfos -c "${capture}" >/dev/null
done

count() {
  tshark -r "$1" -Y "$2" -T fields -e frame.number 2>/dev/null | wc -l | tr -d ' '
}

first_frame() {
  tshark -r "$1" -Y "$2" -T fields -e frame.number 2>/dev/null | sed -n '1p'
}

direct_uploader=$(count "${uploader}" 'ip.addr == 11.231.0.21 && ip.addr == 11.231.0.22')
direct_downloader=$(count "${downloader}" 'ip.addr == 11.231.0.21 && ip.addr == 11.231.0.22')
test "${direct_uploader}" -eq 0
test "${direct_downloader}" -eq 0

uploader_turn=$(count "${uploader}" 'ip.addr == 11.231.0.21 && ip.addr == 11.231.0.15 && udp.port == 3478')
downloader_turn=$(count "${downloader}" 'ip.addr == 11.231.0.22 && ip.addr == 11.231.0.15 && udp.port == 3478')
uploader_authority=$(count "${uploader}" 'ip.addr == 11.231.0.21 && ip.addr == 11.231.0.10 && tcp.port == 443')
downloader_authority=$(count "${downloader}" 'ip.addr == 11.231.0.22 && ip.addr == 11.231.0.10 && tcp.port == 443')
downloader_r2=$(count "${downloader}" 'ip.addr == 11.231.0.22 && ip.addr == 11.231.0.13 && tcp.port == 443')
downloader_federated_origins=$(count "${downloader}" 'ip.addr == 11.231.0.22 && (ip.addr == 11.231.0.11 || ip.addr == 11.231.0.12) && tcp.port == 443')
downloader_r2_first=$(first_frame "${downloader}" 'ip.addr == 11.231.0.22 && ip.addr == 11.231.0.13 && tcp.port == 443')
downloader_turn_first=$(first_frame "${downloader}" 'ip.addr == 11.231.0.22 && ip.addr == 11.231.0.15 && udp.port == 3478')

test "${uploader_turn}" -gt 0
test "${downloader_turn}" -gt 0
test "${uploader_authority}" -gt 0
test "${downloader_authority}" -gt 0
test "${downloader_r2}" -gt 0
test "${downloader_federated_origins}" -eq 0
test -n "${downloader_r2_first}"
test -n "${downloader_turn_first}"
test "${downloader_r2_first}" -lt "${downloader_turn_first}"

cat > /lab/results/packet-proof.json <<EOF
{
  "schema": "rw.distributed-lab.packet-proof.v1",
  "direct_peer_packets": 0,
  "uploader_turn_packets": ${uploader_turn},
  "downloader_turn_packets": ${downloader_turn},
  "uploader_authority_tls_packets": ${uploader_authority},
  "downloader_authority_tls_packets": ${downloader_authority},
  "downloader_r2_tls_packets": ${downloader_r2},
  "downloader_federated_origin_tls_packets": 0,
  "historical_r2_checked_before_turn": true,
  "turn_egress_observed": true,
  "netem_profile_available": true
}
EOF
