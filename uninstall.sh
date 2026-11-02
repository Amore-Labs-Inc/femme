#!/usr/bin/env bash
# Takes FEMME off again. Asks before each part; nothing is deleted without a yes.
#
#   ./uninstall.sh server   services, the board, Caddy's site, femme's files (the agents' work folders stay)
#   ./uninstall.sh laptop   femme-link, Mutagen sync sessions, the herdr plugin/machine, the ~/.ssh/config entry
#   ./uninstall.sh aws      lists what provision.sh made and prints the commands to delete it; runs none of them
set -uo pipefail
cd "$(dirname "$0")"
. ./laptop/lib.sh
[ -f .env ] || die "no .env here: nothing to uninstall from this checkout"
. ./.env

server() {
  ask_yes "Stop and remove FEMME's services and files on $HOST?" n || return 0
  ssh "$HOST" 'set -u
    cd ~/.config/systemd/user 2>/dev/null && for u in femme-deploy.timer femme-deploy.service femme-board femme-worker femme-cswap femme-fable-cap herdr-notify herdr-instinct herdr-server; do
      systemctl --user disable --now "$u" 2>/dev/null; rm -f "$u" "$u.service"; done
    systemctl --user daemon-reload
    rm -f ~/.local/bin/femme-board ~/.local/bin/femme-tui ~/.local/bin/pc ~/.claude/rules/femme-pc.md
    sudo rm -f /usr/local/bin/femme-tui
    herdr plugin unlink femme >/dev/null 2>&1
    sed -i "/^<!-- femme pc start -->$/,/^<!-- femme pc end -->$/d" ~/.codex/AGENTS.md 2>/dev/null
    sed -i "/^# femme pc start$/,/^# femme pc end$/d" ~/.ssh/config 2>/dev/null
    sudo rm -f /etc/ssh/sshd_config.d/20-femme-link.conf && sudo systemctl reload ssh
    if [ -f /etc/caddy/Caddyfile ] && grep -q "127.0.0.1:8787" /etc/caddy/Caddyfile; then
      sudo systemctl disable --now caddy; sudo rm -f /etc/caddy/Caddyfile; fi
    rm -rf ~/femme'
  say "services and files removed. Left on $HOST: herdr, the agents and their logins, ~/femme-work."
  if ask_yes "Also delete the board's data on $HOST (issues, API keys, sessions in ~/.local/state/femme)?" n; then
    ssh "$HOST" 'rm -rf ~/.local/state/femme' && say "board data deleted"
  fi
}

laptop() {
  ask_yes "Remove FEMME from this laptop?" n || return 0
  case $(uname -s) in
    Darwin)
      launchctl bootout "gui/$(id -u)/dev.femme.link" 2>/dev/null
      rm -f ~/Library/LaunchAgents/dev.femme.link.plist ;;
    Linux)
      systemctl --user disable --now femme-link 2>/dev/null
      rm -f ~/.config/systemd/user/femme-link.service
      systemctl --user daemon-reload ;;
  esac
  if command -v mutagen >/dev/null; then
    # Sessions sync-dir.sh made are named femme-<checksum>; both copies of each folder stay.
    for s in $(mutagen sync list 2>/dev/null | awk '/^Name: femme-/ {print $2}'); do mutagen sync terminate "$s"; done
  fi
  if command -v herdr >/dev/null; then
    herdr plugin unlink femme >/dev/null 2>&1
    herdr machine remove "$HOST" >/dev/null 2>&1
  fi
  if grep -q "^# femme $HOST start\$" ~/.ssh/config 2>/dev/null; then
    awk -v s="# femme $HOST start" -v e="# femme $HOST end" '$0==s{skip=1} !skip{print} $0==e{skip=0}' ~/.ssh/config > ~/.ssh/config.tmp \
      && mv ~/.ssh/config.tmp ~/.ssh/config && chmod 600 ~/.ssh/config
  else
    warn "~/.ssh/config: remove the 'Host $HOST' entry by hand if you no longer need it"
  fi
  warn "left in place: the femme key bindings in ~/.config/herdr/config.toml (delete the two femme entries), mutagen.service, and this checkout"
  say "laptop cleaned up"
}

aws_list() {
  command -v aws >/dev/null || die "the AWS CLI is not installed here"
  a() { command aws --region "$REGION" --output text "$@"; }
  say "made by provision.sh in $REGION (tag Name=femme). Nothing is deleted here: run what you want yourself."
  id=$(a ec2 describe-instances --filters Name=tag:Name,Values=femme Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[0].Instances[0].InstanceId')
  alloc=$(a ec2 describe-addresses --filters Name=tag:Name,Values=femme --query 'Addresses[0].AllocationId')
  sg=$(a ec2 describe-security-groups --filters Name=group-name,Values=femme --query 'SecurityGroups[0].GroupId')
  echo
  echo "# Back up anything you need from the server first: its disk is deleted with it."
  [ "$id" = None ] || cat <<EOF
aws --region $REGION ec2 modify-instance-attribute --instance-id $id --no-disable-api-termination
aws --region $REGION ec2 terminate-instances --instance-ids $id
aws --region $REGION ec2 wait instance-terminated --instance-ids $id
EOF
  [ "$alloc" = None ] || echo "aws --region $REGION ec2 release-address --allocation-id $alloc"
  [ "$sg" = None ] || echo "aws --region $REGION ec2 delete-security-group --group-id $sg"
  if command aws iam get-role --role-name femme >/dev/null 2>&1; then cat <<EOF
aws iam remove-role-from-instance-profile --instance-profile-name femme --role-name femme
aws iam delete-instance-profile --instance-profile-name femme
aws iam delete-role-policy --role-name femme --policy-name femme-spawn
aws iam delete-role --role-name femme
EOF
  fi
  [ "$id$alloc$sg" != NoneNoneNone ] || say "nothing tagged femme found in $REGION"
}

case ${1:-} in
  server) server ;;
  laptop) laptop ;;
  aws) aws_list ;;
  *) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
