#!/bin/sh
# git hooks の冪等セットアップ。clone 後に一度だけ `./.githooks/install.sh` を実行する。
# - リポジトリ管理の .githooks.gitconfig を include.path で読み込ませる
# - hook スクリプトに実行権を付与
# 要 git >= 2.55（config ベース hooks）。
set -eu

# git リポジトリ外（tarball 取得時など）では何もしない
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0
cd "$(git rev-parse --show-toplevel)"

# git 2.55 未満なら警告して終了。エラーにはしない（CI が最終的な砦）。
ver=$(git --version | awk '{print $3}')
major=$(printf '%s' "$ver" | cut -d. -f1)
minor=$(printf '%s' "$ver" | cut -d. -f2)
if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 55 ]; }; then
	echo "[hooks] git >= 2.55 is required (found $ver), skipping config-based hooks" >&2
	exit 0
fi

target="../.githooks.gitconfig"
# 既に登録済みなら追加しない（冪等）
if ! git config --local --get-all include.path 2>/dev/null | grep -qx "$target"; then
	git config --local --add include.path "$target"
fi

chmod +x .githooks/pre-push 2>/dev/null || true

echo "[hooks] enabled config-based hooks (pre-push: secret scan)"
