#!/bin/sh
# Starts a node of a ShardRegion's Valkey cluster. POD_IP and POD_HOSTNAME (the pod's name in the headless Service)
# come from the pod. A pod gets a new IP when it restarts, and nodes.conf keeps the IPs the node last knew of itself
# and the others: each is replaced by what the node's hostname resolves to now (its own by POD_IP), so a cluster whose
# pods all restarted finds itself again. A name that does not resolve yet keeps its IP; gossip brings it up to date.
set -eu
conf=/data/nodes.conf
if [ -s "$conf" ]; then
  awk -v self="$POD_IP" '
    $1 == "vars" { print; next }
    {
      # <id> <ip>:<port>@<bus port>,<hostname>,<aux>... <flags> ...
      split($2, parts, ",")
      address = parts[1]
      if (!match(address, /:[0-9]+@/)) { print; next }
      ip = ""
      if ($3 ~ /myself/) ip = self
      else if (parts[2] != "") {
        line = ""
        command = "getent hosts " parts[2]
        command | getline line
        close(command)
        split(line, fields, " ")
        ip = fields[1]
      }
      if (ip != "") { $2 = ip substr($2, RSTART) }
      print
    }' "$conf" > "$conf.new"
  mv "$conf.new" "$conf"
fi
exec valkey-server /etc/valkey/valkey.conf --cluster-announce-hostname "$POD_HOSTNAME"
