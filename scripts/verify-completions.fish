#!/usr/bin/env fish
# Verification script for rearview fish completions
# Static verification & regression guard

set -l script_dir (status dirname)
set -l repo_root (builtin realpath "$script_dir/..")
set -l completion_file "$repo_root/completions/rearview.fish"

if not test -f "$completion_file"
    echo "Error: Completion file not found: $completion_file"
    exit 1
end

set -g fish_complete_path "$repo_root/completions"
echo "Verifying completion script: $completion_file"

# 1. Syntax check
if not fish -n "$completion_file" 2>&1
    echo "FAIL: Syntax error in $completion_file"
    exit 1
end

source "$completion_file"
set -l completions (complete -C "rearview " 2>/dev/null)
echo "  Registered (count $completions) top-level completions"

# 2. Subcommand flag isolation check (No root flag leakage)
set -l sub_name "agent"
if test -n "$sub_name"
    set -l sub_flags (complete -C "rearview $sub_name --" 2>/dev/null | string match -r '^--[a-zA-Z0-9_-]+' | string replace -r '\t.*' '')
    set -l root_flags --show-tools --no-tools --show-dir --show-all-dirs --last --first --show-thinking --hide-thinking --resume --fork-session --show-path --show-id --plain --debug --local --since --after --before --pager --no-pager --render --no-color --delete --debug-search --generate-semantic-cache
    for rf in $root_flags
        if contains -- $rf $sub_flags
            echo "FAIL: Root flag $rf leaked into '$sub_name --'"
            exit 1
        end
    end
end

echo "VERIFICATION PASSED"
exit 0
