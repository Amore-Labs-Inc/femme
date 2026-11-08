# AWS

## The server's own AWS access

By default the server gets no AWS access. With `AWS_ROLE=spawn` in `.env`, `provision.sh` gives the instance a role (`femme`) that may launch, stop and terminate the instance types in `SPAWN_TYPES` in `REGION`, and nothing else. The credentials come from the instance metadata service and rotate themselves, so there is no key on disk; but anything running on the server, agents included, can use them. Grant more by hand only if you accept that.

Set a hard spend brake either way: in Service Quotas, lower "Running On-Demand Standard instances" (vCPUs) for the region. Add a Budgets email alert too, but alerts lag hours and stop nothing.

## Backups

Data Lifecycle Manager snapshots the box's volume (tag `Name=femme`) at 03:00 UTC and keeps the newest seven: policy `femme nightly` in eu-central-1, set up once from a laptop with admin credentials (`aws dlm create-default-role --resource-type snapshot`, then `aws dlm create-lifecycle-policy` with `ResourceTypes VOLUME`, `TargetTags Name=femme`, `Interval 24 HOURS`, `RetainRule Count 7`). It runs on AWS's side, so a hung or full box still gets its snapshot, and the box's own role needs no snapshot rights. Snapshots are incremental: seven cost the used blocks once plus a week of changes, about $0.05 per GB-month. Restore: create an AMI from the newest snapshot, launch it, point the laptop's `HOST` at it.
