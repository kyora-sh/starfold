#!/usr/bin/env bash
# shape.sh <rtt ms> <mbit/s> <port>...: the browser<->server leg of those ports on loopback gets the
# RTT, half each way, and the rate server->client with a bounded queue (2x BDP). Everything else on lo is left alone.
set -eu
rtt=$1 rate=$2; shift 2
delay=$((rtt / 2))
limit=$(( (rate * 125 * rtt * 2 + 1499) / 1500 ))
sudo ip link set lo mtu 1500
sudo ethtool -K lo tso off gso off gro off >/dev/null 2>&1 || true
sudo tc qdisc replace dev lo root handle 1: prio bands 3 priomap 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
sudo tc qdisc add dev lo parent 1:2 handle 20: netem delay ${delay}ms limit 10000
sudo tc qdisc add dev lo parent 1:3 handle 30: netem delay ${delay}ms rate ${rate}mbit limit $limit
for p in "$@"; do
	sudo tc filter add dev lo parent 1: protocol ip prio 1 u32 match ip sport "$p" 0xffff flowid 1:3
	sudo tc filter add dev lo parent 1: protocol ip prio 2 u32 match ip dport "$p" 0xffff flowid 1:2
done
