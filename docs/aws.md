# AWS

## The server's own AWS access

By default the server gets no AWS access. With `AWS_ROLE=spawn` in `.env`, `provision.sh` gives the instance a role (`femme`) that may launch, stop and terminate the instance types in `SPAWN_TYPES` in `REGION`, and nothing else. The credentials come from the instance metadata service and rotate themselves, so there is no key on disk; but anything running on the server, agents included, can use them. Grant more by hand only if you accept that.

Set a hard spend brake either way: in Service Quotas, lower "Running On-Demand Standard instances" (vCPUs) for the region. Add a Budgets email alert too, but alerts lag hours and stop nothing.

## Backups

On EC2 (the aws CLI is on PATH), `bootstrap.sh` installs `femme-snapshot.timer`: at 03:00 UTC `server/snapshot.sh` takes an EBS snapshot of every volume attached to the box and keeps the newest seven per volume, so a box whose snapshots stop coming keeps its last seven. Snapshots are incremental, so seven cost the used blocks once plus a week of changes (about $0.05 per GB-month); hourly ones would also keep every rewrite of the build directories, which is why it is nightly. The instance role needs `ec2:CreateSnapshot`, `CreateTags`, `DescribeVolumes`, `DescribeSnapshots` and `DeleteSnapshot`. Restore: launch a new instance from the snapshot (create an AMI from it, or an image from the volume), then point the laptop's `HOST` at it. `systemctl --user start femme-snapshot` takes one now; `journalctl --user -u femme-snapshot` shows what it did.
