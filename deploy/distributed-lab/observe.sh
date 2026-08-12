#!/bin/sh
set -eu

role="${1:?observer role is required}"
seed="${2:?netem seed is required}"
capture="/lab/results/captures/${role}.pcap"
ready="/lab/control/observer-${role}.ready"

rm -f "${capture}" "${ready}"

# Only client-to-TURN UDP is selected. `LAB_ENABLE_NETEM=1` opts into the
# deterministic impairment profile after the baseline relay gate is green.
tc qdisc replace dev eth0 root handle 1: prio bands 3
if [ "${LAB_ENABLE_NETEM:-0}" = 1 ]; then
  tc qdisc replace dev eth0 parent 1:3 handle 30: netem \
    limit 1000 delay 15ms 4ms 25% loss random 2% 10% \
    duplicate 1% 10% reorder 12% 25% seed "${seed}"
else
  tc qdisc replace dev eth0 parent 1:3 handle 30: netem seed "${seed}"
fi
tc filter replace dev eth0 protocol ip parent 1: prio 1 u32 \
  match ip protocol 17 0xff \
  match ip dport 3478 0xffff \
  flowid 1:3

cleanup() {
  rm -f "${ready}"
  if [ -n "${capture_pid:-}" ]; then
    kill "${capture_pid}" >/dev/null 2>&1 || true
    wait "${capture_pid}" >/dev/null 2>&1 || true
  fi
  tc qdisc del dev eth0 root >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

tcpdump --immediate-mode -i eth0 -U -n -s 0 -w "${capture}" 'ip' &
capture_pid=$!
sleep 1
kill -0 "${capture_pid}"
printf 'ready\n' > "${ready}"
wait "${capture_pid}"
