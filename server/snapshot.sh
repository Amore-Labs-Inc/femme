#!/usr/bin/env bash
# Nightly EBS snapshot of every volume attached to this box, from femme-snapshot.timer (bootstrap.sh
# installs it where the aws CLI is on PATH, so only on EC2). Keeps the newest KEEP (7) snapshots per
# volume and deletes the rest, and only after the new one was requested: a box whose snapshots stop
# being made keeps its last seven instead of pruning down to none. Snapshots are incremental, so seven
# cost the used blocks once plus a week of changed blocks. Crash-consistent, like a power cut: SQLite
# (WAL) and git recover from that. The instance role needs ec2:CreateSnapshot, CreateTags,
# DescribeVolumes, DescribeSnapshots and DeleteSnapshot.
set -euo pipefail
export AWS_PAGER=
tok=$(curl -sf -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
md() { curl -sf -H "X-aws-ec2-metadata-token: $tok" "http://169.254.169.254/latest/meta-data/$1"; }
instance=$(md instance-id)
AWS_DEFAULT_REGION=$(md placement/region); export AWS_DEFAULT_REGION
sync
for vol in $(aws ec2 describe-volumes --filters "Name=attachment.instance-id,Values=$instance" --query 'Volumes[].VolumeId' --output text); do
  snap=$(aws ec2 create-snapshot --volume-id "$vol" --description "femme nightly $instance $(date -u +%F)" \
    --tag-specifications "ResourceType=snapshot,Tags=[{Key=femme-backup,Value=$instance}]" --query SnapshotId --output text)
  echo "femme-snapshot: $vol -> $snap"
  # Oldest first; everything but the newest KEEP goes. The one just requested counts as kept.
  aws ec2 describe-snapshots --owner-ids self --filters "Name=tag:femme-backup,Values=$instance" "Name=volume-id,Values=$vol" \
    --query 'sort_by(Snapshots,&StartTime)[].SnapshotId' --output text | tr '\t' '\n' | head -n -"${KEEP:-7}" |
    while read -r old; do [ -z "$old" ] || { aws ec2 delete-snapshot --snapshot-id "$old"; echo "femme-snapshot: deleted $old"; }; done
done
