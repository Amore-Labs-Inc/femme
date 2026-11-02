#!/bin/sh
# usage: ssh.sh <cmd...>   (IP read from ./ip)
D=$(dirname "$0")
exec ssh -i "$D/femme-omarchy.pem" -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$D/known_hosts" -o ConnectTimeout=10 -o ServerAliveInterval=30 arch@"$(cat "$D/ip")" "$@"
