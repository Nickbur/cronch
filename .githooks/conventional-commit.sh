#!/bin/sh
# commit-msg: enforce Conventional Commits on the header line.
#   type(scope)!: subject      e.g.  feat(auth): add passkey sign-in
#   types: build chore ci docs feat fix perf refactor revert style test
# Merge / Revert / fixup! / squash! / amend! headers pass untouched.
file=$1
header=$(grep -v '^#' "$file" | sed -n '/[^[:space:]]/{p;q;}')

case "$header" in
    'Merge '* | 'Revert "'* | 'fixup! '* | 'squash! '* | 'amend! '*) exit 0 ;;
esac

types='build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test'
if ! printf '%s\n' "$header" | grep -qE "^($types)(\([a-z0-9._/-]+\))?!?: [^ ].*"; then
    {
        echo "commit-msg: the header must follow Conventional Commits:"
        echo "    type(scope)!: subject"
        echo "  types: build chore ci docs feat fix perf refactor revert style test"
        echo "  scope (optional): lower-case, digits, . _ / -"
        echo "  examples:  fix(auth): refresh the token before it expires"
        echo "             chore: update dependencies"
        echo "  got: $header"
    } >&2
    exit 1
fi

if [ "${#header}" -gt 100 ]; then
    echo "commit-msg: header is ${#header} characters; keep it within 100." >&2
    exit 1
fi
exit 0
