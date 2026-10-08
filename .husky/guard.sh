#!/bin/sh
# Pre-commit garbage guard — rejects staged files that are almost always accidents:
#   1. stray names from a mistyped shell command ("{", "({", "console.log('x'" …)
#   2. new empty files without an extension (e.g. "id"); .gitkeep and friends are fine
#   3. files over 2 MB, unless listed (one glob per line) in large-files.txt next to this script
#   4. unresolved merge-conflict markers
# Bypass for a deliberate exception: git commit --no-verify
dir=$(dirname "$0")
allow="$dir/large-files.txt"
limit=2097152
tab=$(printf '\t')

# Each problem prints one line; the loop runs in a subshell, so problems are collected as output.
# Case patterns inside $( ) keep the optional leading "(": bash 3.2 (macOS /bin/sh) misreads a bare ")".
problems=$(
    git -c core.quotePath=false diff --cached --name-status --diff-filter=ACMR |
        while IFS="$tab" read -r status path rest; do
            # Renames/copies carry "old<TAB>new"; check the new path.
            case "$status" in (R* | C*) path=$rest ;; esac
            name=${path##*/}

            case "$name" in
                (*[\'\"\`{}\;\|\<\>]* | *=\>*) echo "suspicious file name: $path" ;;
            esac
            opened=$(($(printf '%s' "$name" | tr -cd '([' | wc -c)))
            closed=$(($(printf '%s' "$name" | tr -cd ')]' | wc -c)))
            [ "$opened" -ne "$closed" ] && echo "unbalanced brackets in file name: $path"

            size=$(($(git cat-file -s ":$path" 2>/dev/null || echo 0)))
            if [ "$status" = A ] && [ "$size" -eq 0 ]; then
                case "$name" in
                    (.gitkeep | .keep | .nojekyll | __init__.py | py.typed | *.*) ;;
                    (*) echo "new empty file without an extension: $path" ;;
                esac
            fi

            if [ "$size" -gt "$limit" ]; then
                allowed=0
                if [ -f "$allow" ]; then
                    while IFS= read -r pattern; do
                        case "$pattern" in ('' | '#'*) continue ;; esac
                        # shellcheck disable=SC2254
                        case "$path" in ($pattern) allowed=1 ;; esac
                    done <"$allow"
                fi
                [ "$allowed" = 1 ] || echo "file over 2 MB: $path ($size bytes) — list it in $allow if intended"
            fi
        done
    # Only added lines count, so committing a resolution that removes markers passes.
    git -c core.quotePath=false diff --cached -U0 --no-color |
        awk '/^\+\+\+ b\// { f = substr($0, 7) } /^\+(<<<<<<<|>>>>>>>) / { print "conflict markers in: " f }' |
        sort -u
)

[ -z "$problems" ] && exit 0
printf '%s\n' "$problems" | sed 's/^/guard: /' >&2
echo "guard: commit aborted (bypass a deliberate exception with --no-verify)" >&2
exit 1
