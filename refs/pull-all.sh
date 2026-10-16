#!/usr/bin/env bash
# 更新 refs/ 下的所有参考库：对每个子目录里的 git 仓库执行一次 git pull。
#
# 默认带 --ff-only：本地有分叉或未提交改动时直接失败并报出来，不会悄悄产生
# 合并提交或改写这些只读参考库的历史。想换行为就在参数里追加 git pull 的选项。
#
# 用法: ./refs/pull-all.sh [额外传给 git pull 的参数]
set -o pipefail

refs_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$refs_dir" || exit 1

ok=()
failed=()

for dir in */; do
    dir="${dir%/}"
    [ -d "$dir/.git" ] || continue
    printf '==> %s\n' "$dir"
    if git -C "$dir" pull --ff-only "$@"; then
        ok+=("$dir")
    else
        failed+=("$dir")
    fi
    printf '\n'
done

printf '更新 %d 个' "${#ok[@]}"
if [ "${#ok[@]}" -gt 0 ]; then
    printf ': %s' "${ok[*]}"
fi
printf '\n'

if [ "${#failed[@]}" -gt 0 ]; then
    printf '失败 %d 个: %s\n' "${#failed[@]}" "${failed[*]}" >&2
    exit 1
fi