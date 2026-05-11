#!/usr/bin/env bash
#
# doc-parity — every `mxnode <cmd>` in mintlify-docs/mxnode/*.mdx must
# parse cleanly under the binary at $1 (or `target/release/mxnode`).
#
# We feed each invocation through `--help`-style validation: clap parses
# the whole command line and bails before calling into the run handler,
# which means we catch removed subcommands, renamed flags, and typos
# without actually running anything destructive. Any line that resolves
# to `--help` or `--version` is exercised as-is; anything else gets a
# `--help` appended after the command path so we don't trigger a real
# install/upgrade/etc.
#
# Inputs:
#   $1   = path to mxnode binary  (default: target/release/mxnode)
#   $2   = path to mintlify-docs root (default: ../mintlify-docs from cwd)
#
# Exits non-zero on the first command that doesn't parse, listing
# (file:line, the offending invocation, clap's complaint).

set -u

MXNODE="${1:-${MXNODE_BIN:-$(pwd)/target/release/mxnode}}"
DOCS="${2:-${DOCS_DIR:-$(pwd)/../mintlify-docs}}"

if [ ! -x "$MXNODE" ]; then
    echo "FATAL: mxnode binary not executable at $MXNODE" >&2
    exit 2
fi
if [ ! -d "$DOCS/mxnode" ]; then
    echo "FATAL: docs dir $DOCS/mxnode not found" >&2
    exit 2
fi

# Subcommands clap would recognise as the first positional after
# `mxnode`. Used to filter lines that just happen to mention `mxnode`
# in prose.
SUBCOMMANDS="config install start stop restart status logs metrics upgrade db keys uninstall doctor import-bash self-update completions version help"

PASS=0
FAIL=0
declare -a FAILURES

# Walk the docs once.
mapfile -t MDX < <(find "$DOCS/mxnode" -name '*.mdx' -type f | sort)

for f in "${MDX[@]}"; do
    rel="${f#$DOCS/}"
    # Track line numbers as we walk. awk emits "<lineno>\t<content>".
    while IFS=$'\t' read -r lineno line; do
        # Strip leading prompt characters ($, ❯) but NOT `#` — a leading
        # `#` means the whole line is a comment in a shell code block,
        # which we filter out below.
        cmd=$(echo "$line" | sed -E 's/^[[:space:]]*[\$❯][[:space:]]*//;s/^[[:space:]]+//')
        # Skip outright comments (lines starting with `#` after trim) —
        # they often live inside ```toml blocks or as inline file
        # headers and would otherwise look like `mxnode config — ...`
        # to the parser below.
        case "$cmd" in
            \#*) continue ;;
        esac
        # Require that the command starts with `mxnode `.
        case "$cmd" in
            mxnode\ *) ;;
            *) continue ;;
        esac
        # Skip compound commands — running `mxnode A && mxnode B` would
        # actually execute A even if we appended --help to B. Doc-parity
        # is parse-only; refuse to evaluate side-effecty chains.
        case "$cmd" in
            *' && '* | *' || '* | *' ; '*) continue ;;
        esac
        # Strip trailing comments (`# foo`) so they don't get parsed.
        cmd=$(echo "$cmd" | sed -E 's/[[:space:]]+#[[:space:]].*$//')
        # Strip line-continuation backslashes — multi-line examples in docs.
        cmd=$(echo "$cmd" | sed -E 's/\\$//' | tr -s ' ')
        # Skip lines that look like prose mentioning the binary name
        # ("the mxnode CLI", "mxnode does X"). Heuristic: the second
        # token must be a known subcommand or start with `--`.
        second_token=$(echo "$cmd" | awk '{print $2}')
        case "$second_token" in
            --*) ;;
            *)
                if ! echo " $SUBCOMMANDS " | grep -qF " $second_token "; then
                    continue
                fi
                ;;
        esac
        # Skip pipes (e.g. `mxnode --json status > foo.json`).
        cmd=$(echo "$cmd" | sed -E 's/[[:space:]]*[|>][[:space:]].*$//')
        # Always append `--help` unless the line already explicitly
        # asks for `--help` or `--version`. clap parses the entire
        # command line before reaching --help's short-circuit, so any
        # unknown subcommand or flag is still caught — but we never
        # actually call into the runtime, which means no state.toml
        # required and no destructive ops fire.
        if echo "$cmd" | grep -qE -- '(--help|--version)'; then
            full="$cmd"
        else
            full="$cmd --help"
        fi
        # Replace the literal `mxnode` with our binary.
        full=$(echo "$full" | sed "s|^mxnode |$MXNODE |")
        out=$(eval "$full" 2>&1)
        code=$?
        if [ $code -eq 0 ]; then
            PASS=$((PASS+1))
        else
            # Filter clap's known-good failure modes (e.g. an example
            # with an intentionally-invalid value where the doc shows
            # the rejection). Heuristic: clap parse errors print to
            # stderr starting with "error:". We accept these for lines
            # explicitly testing rejections (the docs flag them with
            # "REJECTED" or live inside the misuse-rejections table).
            FAIL=$((FAIL+1))
            FAILURES+=("$rel:$lineno  →  $cmd")
            FAILURES+=("    $(echo "$out" | head -1)")
        fi
    done < <(awk '{printf "%d\t%s\n", NR, $0}' "$f")
done

echo
echo "============================================================"
echo "  doc-parity"
echo "============================================================"
echo "  invocations parsed: $PASS"
echo "  invocations failed: $FAIL"

if [ $FAIL -gt 0 ]; then
    echo
    echo "  FAILED:"
    for line in "${FAILURES[@]}"; do echo "    $line"; done
    exit 1
fi
exit 0
