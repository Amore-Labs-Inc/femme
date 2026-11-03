#!/usr/bin/env bash
# Create the VPS on AWS (default VPC). Safe to rerun: reuses everything tagged femme.
set -euo pipefail
cd "$(dirname "$0")/.."  # the checkout root: .env and every path below are relative to it
[ -f .env ] || cp .env.example .env
. ./.env
. ./laptop/lib.sh
aws() { command aws --region "$REGION" --output text "$@"; }
tag() { echo "ResourceType=$1,Tags=[{Key=Name,Value=femme}]"; }
retry() {  # rerun while EC2 has not yet heard of the new instance profile (takes a few seconds)
  for _ in 1 2 3 4 5 6; do
    { err=$("$@" 2>&1 >&3); } 3>&1 && return
    [[ $err = *"Invalid IAM Instance Profile"* ]] || break
    sleep 5
  done
  echo "$err" >&2; return 1
}

[ -n "$NTFY_TOPIC" ] || { NTFY_TOPIC=femme-$(openssl rand -hex 16); setenv NTFY_TOPIC "$NTFY_TOPIC"; }

[ -n "$(aws ec2 describe-instance-type-offerings --filters "Name=instance-type,Values=$INSTANCE_TYPE" --query 'InstanceTypeOfferings[].InstanceType')" ] \
  || { echo "$INSTANCE_TYPE is not offered in $REGION" >&2; exit 1; }

sg=$(aws ec2 describe-security-groups --filters Name=group-name,Values=femme --query 'SecurityGroups[0].GroupId')
if [ "$sg" = None ]; then
  sg=$(aws ec2 create-security-group --group-name femme --description "femme ssh" --query GroupId)
  aws ec2 authorize-security-group-ingress --group-id "$sg" --ip-permissions \
    'IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=0.0.0.0/0}]' >/dev/null
fi
if [ "${BOARD_PUBLIC:-0}" = 1 ]; then  # public board (opt-in): Caddy needs 80 for the certificate challenge, 443 to serve
  aws ec2 authorize-security-group-ingress --group-id "$sg" --ip-permissions \
    'IpProtocol=tcp,FromPort=80,ToPort=80,IpRanges=[{CidrIp=0.0.0.0/0}]' \
    'IpProtocol=tcp,FromPort=443,ToPort=443,IpRanges=[{CidrIp=0.0.0.0/0}]' >/dev/null 2>&1 || true  # already open
fi
if [ "${MOSH:-0}" = 1 ]; then  # phone terminal (opt-in): mosh picks a UDP port in this range
  aws ec2 authorize-security-group-ingress --group-id "$sg" --ip-permissions \
    'IpProtocol=udp,FromPort=60000,ToPort=61000,IpRanges=[{CidrIp=0.0.0.0/0}]' >/dev/null 2>&1 || true  # already open
fi
if [ "${FEMMED:-0}" != 0 ]; then  # the desktop app's QUIC transport (docs/plan-desktop.md); ssh is its fallback
  aws ec2 authorize-security-group-ingress --group-id "$sg" --ip-permissions \
    "IpProtocol=udp,FromPort=${QUIC_PORT:-7788},ToPort=${QUIC_PORT:-7788},IpRanges=[{CidrIp=0.0.0.0/0}]" >/dev/null 2>&1 || true  # already open
fi

# Optional instance role (AWS_ROLE=spawn): agents on the box may launch small instances in this region
# and nothing else. Off by default: the box gets no AWS access. Policy rewritten every run.
profile=()
if [ "${AWS_ROLE:-none}" = spawn ]; then
types=$(jq -Rc 'split(",")' <<<"${SPAWN_TYPES:-t3.small,t3.medium,t3.large}")
policy='{"Version":"2012-10-17","Statement":[
  {"Effect":"Allow","Resource":"*","Action":["ec2:Describe*","ec2:RunInstances","ec2:CreateTags","ec2:StartInstances","ec2:StopInstances","ec2:TerminateInstances","ec2:CreateSecurityGroup","ec2:AuthorizeSecurityGroupIngress","ec2:CreateKeyPair"]},
  {"Effect":"Deny","Action":"ec2:RunInstances","Resource":"arn:aws:ec2:*:*:instance/*","Condition":{"StringNotEquals":{"ec2:InstanceType":'"$types"'}}},
  {"Effect":"Deny","Action":"ec2:*","Resource":"*","Condition":{"StringNotEquals":{"aws:RequestedRegion":"'"$REGION"'"}}}]}'
aws iam get-role --role-name femme >/dev/null 2>&1 || aws iam create-role --role-name femme --assume-role-policy-document \
  '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
aws iam put-role-policy --role-name femme --policy-name femme-spawn --policy-document "$policy"
roles=$(aws iam get-instance-profile --instance-profile-name femme --query 'InstanceProfile.Roles[].RoleName' 2>/dev/null) \
  || { aws iam create-instance-profile --instance-profile-name femme >/dev/null; roles=; }
[ "$roles" = femme ] || aws iam add-role-to-instance-profile --instance-profile-name femme --role-name femme
profile=(--iam-instance-profile Name=femme)
fi

id=$(aws ec2 describe-instances --query 'Reservations[0].Instances[0].InstanceId' \
  --filters Name=tag:Name,Values=femme Name=instance-state-name,Values=pending,running,stopping,stopped)
if [ "$id" = None ]; then
  ami=$(aws ssm get-parameter --query Parameter.Value \
    --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id)
  userdata=$(mktemp)
  trap 'rm -f "$userdata"' EXIT
  sed -e "s|__USER__|$USER|g" -e "s|__HOST__|$HOST|g" -e "s|__PUBKEY__|$(cat ~/.ssh/id_ed25519.pub)|" laptop/cloud-init.yaml > "$userdata"
  id=$(retry aws ec2 run-instances --image-id "$ami" --instance-type "$INSTANCE_TYPE" \
    --security-group-ids "$sg" --user-data "file://$userdata" ${profile[@]+"${profile[@]}"} \
    --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=$DISK_GB,VolumeType=gp3}" \
    --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1 --disable-api-termination \
    --tag-specifications "$(tag instance)" "$(tag volume)" --query 'Instances[0].InstanceId')
else
  if [ ${#profile[@]} -gt 0 ] && [ "$(aws ec2 describe-iam-instance-profile-associations --filters "Name=instance-id,Values=$id" Name=state,Values=associated \
        --query 'IamInstanceProfileAssociations[0].IamInstanceProfile.Arn')" = None ]; then
    retry aws ec2 associate-iam-instance-profile --instance-id "$id" --iam-instance-profile Name=femme >/dev/null
  fi
  aws ec2 modify-instance-metadata-options --instance-id "$id" --http-tokens required --http-put-response-hop-limit 1 >/dev/null
fi
aws ec2 wait instance-running --instance-ids "$id"

read -r alloc ip < <(aws ec2 describe-addresses --filters Name=tag:Name,Values=femme --query 'Addresses[0].[AllocationId,PublicIp]')
if [ "$alloc" = None ]; then
  read -r alloc ip < <(aws ec2 allocate-address --domain vpc --tag-specifications "$(tag elastic-ip)" --query '[AllocationId,PublicIp]')
fi
aws ec2 associate-address --instance-id "$id" --allocation-id "$alloc" >/dev/null
setenv IP "$ip"

ssh_config_block "$HOST" "$ip" "$USER"
echo "$HOST is $ip ($id). Next: ssh $HOST cloud-init status --wait && ./laptop/sync.sh"
