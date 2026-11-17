#!/bin/sh
# Forms the region's Valkey cluster, or brings it to the nodes it has now: NODES are the pods' host names in ordinal
# order, REPLICAS the replicas per primary. Run again, it changes nothing on a formed cluster.
set -eu
port=6379
cli() { valkey-cli -p "$port" "$@"; }
info() { cli -h "$1" cluster info | tr -d '\r' | awk -F: -v key="$2" '$1 == key { print $2 }'; }
resolve() { getent hosts "$1" | awk '{ print $1; exit }'; }
addresses=""
for host in $NODES; do
  ip=$(resolve "$host" || true)
  until [ -n "$ip" ] && cli -h "$ip" ping >/dev/null 2>&1; do
    echo "waiting for $host"
    sleep 2
    ip=$(resolve "$host" || true)
  done
  addresses="$addresses $ip"
done
set -- $addresses
count=$#
joined=""
fresh=""
for ip in "$@"; do
  if [ "$(info "$ip" cluster_known_nodes)" -gt 1 ] || [ "$(info "$ip" cluster_slots_assigned)" -gt 0 ]; then
    joined="$joined $ip"
  else
    fresh="$fresh $ip"
  fi
done
if [ -z "$joined" ]; then
  echo "creating the cluster:$addresses, $REPLICAS replica(s) per primary"
  cli --cluster create $(for ip in "$@"; do printf '%s:%s ' "$ip" "$port"; done) --cluster-replicas "$REPLICAS" --cluster-yes
else
  set -- $joined
  entry=$1
  cli --cluster check "$entry:$port" >/dev/null || cli --cluster fix "$entry:$port" --cluster-yes
  for ip in $fresh; do
    echo "adding $ip"
    cli --cluster add-node "$ip:$port" "$entry:$port" >/dev/null
  done
  for ip in $addresses; do
    until [ "$(info "$ip" cluster_known_nodes)" -eq "$count" ]; do echo "waiting for $ip to meet every node"; sleep 2; done
  done
  # Nodes without slots or a primary: as many become primaries as the layout lacks, the rest replicas of the primaries
  # with the fewest.
  nodes() { cli -h "$entry" cluster nodes | tr -d '\r'; }
  wanted=$(( count / (REPLICAS + 1) ))
  holding=$(nodes | awk '$3 ~ /master/ && $9 != "" { n++ } END { print n + 0 }')
  need=$(( wanted - holding ))
  keep=""
  for id in $(nodes | awk '$3 ~ /master/ && $9 == "" { print $1 }' | sort); do
    if [ "$need" -gt 0 ]; then
      keep="$keep $id"
      need=$(( need - 1 ))
      continue
    fi
    primary=$(nodes | awk -v keep="$keep" '
      BEGIN { split(keep, kept, " "); for (k in kept) primaries[kept[k]] = 0 }
      $3 ~ /master/ && $9 != "" { primaries[$1] += 0 }
      $3 ~ /slave/ { replicas[$4]++ }
      END { for (p in primaries) print replicas[p] + 0, p }' | sort -n | head -1 | cut -d" " -f2)
    host=$(nodes | awk -v id="$id" '$1 == id { match($2, /:[0-9]+@/); print substr($2, 1, RSTART - 1) }')
    echo "$host becomes a replica of $primary"
    cli -h "$host" cluster replicate "$primary"
  done
  if [ -n "$keep" ]; then
    until cli --cluster check "$entry:$port" >/dev/null; do echo "waiting for the nodes to agree"; sleep 2; done
    echo "moving slots to the new primaries:$keep"
    cli --cluster rebalance "$entry:$port" --cluster-use-empty-masters --cluster-yes
  fi
fi
for ip in $addresses; do
  until [ "$(info "$ip" cluster_state)" = ok ]; do echo "waiting for $ip to see the cluster ok"; sleep 2; done
done
echo "cluster ready"
