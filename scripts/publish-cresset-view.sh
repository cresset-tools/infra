#!/usr/bin/env bash
set -euo pipefail

source_repo="${CRESSET_VIEW_SOURCE:-/home/jelle/cresset}"
target="${CRESSET_VIEW_TARGET:-root@internal.cresset.tools}"
repository_root="/var/lib/cresset-view/repository"

operation_id() {
  jj --ignore-working-copy -R "$source_repo" op log --no-graph -n 1 \
    -T 'id ++ "\n"'
}

before="$(operation_id)"
version="${before:0:16}"
remote_staging="$repository_root/$version"

ssh "$target" mkdir -p "$remote_staging"
rsync -a --delete "$source_repo/.jj/" "$target:$remote_staging/.jj/"
rsync -a --delete "$source_repo/.git/" "$target:$remote_staging/.git/"

after="$(operation_id)"
if [[ "$before" != "$after" ]]; then
  printf 'repository operation changed during publication; retrying is safe\n' >&2
  exit 1
fi

remote_operation="$({
  ssh "$target" chown -R cresset-view:cresset-view "$remote_staging"
  ssh "$target" sudo -u cresset-view cresset-view \
    --repository "$remote_staging" --check
})"

if [[ "$remote_operation" != "$before" ]]; then
  printf 'published operation mismatch: expected %s, got %s\n' \
    "$before" "$remote_operation" >&2
  exit 1
fi

ssh "$target" ln -sfn "$version" "$repository_root/current.next"
ssh "$target" mv -Tf "$repository_root/current.next" "$repository_root/current"
ssh "$target" systemctl restart cresset-view.service

printf 'published jj operation %s\n' "$before"
