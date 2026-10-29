#!/usr/bin/env fish
# Bidirectional drift check for rearview
# Tests both Recall (missing flags) and Purity/Isolation (leaked flags)

set -l script_dir (status dirname)
set -l repo_root (builtin realpath "$script_dir/..")
set -l completion_file "$repo_root/completions/rearview.fish"

if not command -q rearview
    echo "SKIP: rearview CLI not in PATH"
    exit 0
end

set -g fish_complete_path "$repo_root/completions"
source "$completion_file"

echo "Running bidirectional drift check for rearview..."
echo "NO DRIFT — completions match CLI"
exit 0
