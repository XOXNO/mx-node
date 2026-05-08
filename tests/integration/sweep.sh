#!/usr/bin/env bash
#
# mxnode integration sweep — runs the full P-phase matrix from `tests/`
# on a Linux host with systemd. Idempotent: every install is paired
# with a cleanup, and the script asserts the host is pristine at the
# end. Designed to be invoked from CI (`runs-on: ubuntu-latest`) and
# locally during development.
#
# Required: bash 4+, curl, jq, sudo, systemctl, journalctl, file.
# Optional: MXNODE_GITHUB_TOKEN env var (skips GitHub rate limits).
# Inputs:   $1 = path to the mxnode binary to test (defaults to
#                 `target/release/mxnode` relative to repo root).
#
# Pinned upstream tags so the sweep is deterministic across runs.
NODE_TAG="${NODE_TAG:-v1.11.5}"
CONFIG_TAG="${CONFIG_TAG:-v1.11.5.0}"
PROXY_TAG="${PROXY_TAG:-v1.3.4}"

set -u  # strict-undefined; we DO want to handle command failures per-phase

MXNODE="${1:-${MXNODE_BIN:-$(pwd)/target/release/mxnode}}"
if [ ! -x "$MXNODE" ]; then
    echo "FATAL: mxnode binary not executable at $MXNODE" >&2
    exit 2
fi

# Per-phase counters. Aggregated and printed in the trailing summary.
PASS=0
FAIL=0
WARN=0
declare -a FAILED_TESTS

assert() {
    local label="$1"
    local cmd="$2"
    local expect_zero="${3:-1}"  # 1 = expect exit 0; 0 = expect non-zero
    local out
    out=$(bash -c "$cmd" 2>&1)
    local code=$?
    if [ "$expect_zero" -eq 1 ] && [ $code -eq 0 ]; then
        echo "  ✓ $label"
        PASS=$((PASS+1))
    elif [ "$expect_zero" -eq 0 ] && [ $code -ne 0 ]; then
        echo "  ✓ $label (rejected as expected)"
        PASS=$((PASS+1))
    else
        echo "  ✗ $label  (exit=$code, expected $([ "$expect_zero" -eq 1 ] && echo 0 || echo non-0))"
        echo "$out" | head -3 | sed 's/^/    | /'
        FAIL=$((FAIL+1))
        FAILED_TESTS+=("$label")
    fi
}

assert_contains() {
    local label="$1"
    local cmd="$2"
    local needle="$3"
    local out
    out=$(bash -c "$cmd" 2>&1)
    if echo "$out" | grep -qF -- "$needle"; then
        echo "  ✓ $label"
        PASS=$((PASS+1))
    else
        echo "  ✗ $label  (output did not contain: $needle)"
        echo "$out" | head -3 | sed 's/^/    | /'
        FAIL=$((FAIL+1))
        FAILED_TESTS+=("$label")
    fi
}

phase() {
    echo
    echo "============================================================"
    echo "  $1"
    echo "============================================================"
}

# Ensure the box is pristine before we start.
ensure_pristine() {
    local stale=()
    for p in "$HOME/.config/mxnode" "$HOME/.local/state/mxnode" "$HOME/mxnode" "$HOME/elrond-nodes" "$HOME/elrond-utils" "$HOME/elrond-proxy"; do
        [ -e "$p" ] && stale+=("$p")
    done
    local units
    units=$(ls /etc/systemd/system/elrond-*.service 2>/dev/null | head)
    if [ ${#stale[@]} -gt 0 ] || [ -n "$units" ]; then
        echo "host has leftover state — running cleanup first"
        "$MXNODE" cleanup --yes --execute >/dev/null 2>&1 || true
    fi
}

cleanup_artifacts() {
    rm -f "$HOME/VALIDATOR_KEYS/allValidatorsKeys.pem" "$HOME/VALIDATOR_KEYS/node-0.zip" 2>/dev/null
    rmdir "$HOME/VALIDATOR_KEYS" 2>/dev/null || true
    rm -rf /tmp/mxnode-test-keys 2>/dev/null
}

# ============================================================
# P0  pristine baseline
# ============================================================
phase "P0  pristine baseline"
ensure_pristine
assert "no mxnode footprint" "[ ! -e $HOME/.config/mxnode ] && [ ! -e $HOME/.local/state/mxnode ] && [ ! -e $HOME/mxnode ]"
assert "no elrond units"      "! ls /etc/systemd/system/elrond-*.service >/dev/null 2>&1"

# ============================================================
# P1  every command + subcommand renders --help
# ============================================================
phase "P1  --help for every command + subcommand"
for cmd in config install add-nodes start stop restart status logs metrics upgrade db benchmark keygen keys reapply-config dashboard cleanup doctor version; do
    assert "mxnode $cmd --help" "$MXNODE $cmd --help >/dev/null"
done
for s in show get set edit validate; do
    assert "mxnode config $s --help" "$MXNODE config $s --help >/dev/null"
done
for s in prune remove reseed; do
    assert "mxnode db $s --help" "$MXNODE db $s --help >/dev/null"
done
assert "mxnode keys check --help" "$MXNODE keys check --help >/dev/null"
for s in proxy squad; do
    assert "mxnode upgrade $s --help" "$MXNODE upgrade $s --help >/dev/null"
done

# ============================================================
# P2  --version forms
# ============================================================
phase "P2  --version forms"
assert_contains "mxnode --version"  "$MXNODE --version"  "mxnode "
assert_contains "mxnode -V"          "$MXNODE -V"        "mxnode "
assert_contains "mxnode version"     "$MXNODE version"   "mxnode "
assert_contains "mxnode --json version" "$MXNODE --json version" '"version"'

# ============================================================
# P3  benchmark standalone (no install required)
# ============================================================
phase "P3  benchmark works without an install"
assert         "benchmark exits 0 on a host that meets the floor"  "$MXNODE benchmark"
assert_contains "benchmark prints CPU check"  "$MXNODE benchmark"  "[cpu]"
assert_contains "benchmark prints memory check"  "$MXNODE benchmark"  "[memory]"
assert_contains "benchmark --json shape"  "$MXNODE --json benchmark"  '"checks"'

# ============================================================
# P4  auto-init triggered on first state-changing command
# ============================================================
phase "P4  auto-init on first use"
assert "status auto-inits config" "$MXNODE status >/dev/null 2>&1; [ -f $HOME/.config/mxnode/mxnode.toml ]"
assert_contains "default network is mainnet" "$MXNODE config get network.environment" "mainnet"
assert_contains "custom_user detected" "$MXNODE config get paths.custom_user" "$USER"

# ============================================================
# P5  config show / get / set / edit / validate
# ============================================================
phase "P5  config — every subcommand"
assert "config show (toml)"        "$MXNODE config show >/dev/null"
assert "config show --origin"       "$MXNODE config show --origin >/dev/null"
assert_contains "config show --format json" "$MXNODE config show --format json" '"network"'
assert "config get leaf"            "$MXNODE config get install.binary_keep >/dev/null"
assert "config get unknown rejects" "$MXNODE config get nope.nope" 0
assert "config validate"            "$MXNODE config validate"
assert "config validate --strict"   "$MXNODE config validate --strict"
assert "config set string"          "$MXNODE config set network.environment testnet"
assert_contains "config set persists" "$MXNODE config get network.environment" "testnet"
assert "config set int"              "$MXNODE config set install.binary_keep 5"
assert_contains "config set int persists" "$MXNODE config get install.binary_keep" "5"
assert "config set restore" "$MXNODE config set network.environment mainnet"
assert "config set restore int" "$MXNODE config set install.binary_keep 3"
assert "config edit (EDITOR=true)" "EDITOR=true $MXNODE config edit"

# ============================================================
# P6  doctor (table + json)
# ============================================================
phase "P6  doctor"
assert "doctor exits 0 (env is fine; mxnode.toml warning is allowed)" "$MXNODE doctor"
assert_contains "doctor prints platform check" "$MXNODE doctor" "[platform]"
assert_contains "doctor --json shape"           "$MXNODE --json doctor" '"findings"'

# ============================================================
# P7  install rejection matrix (8 misuse combos)
# ============================================================
phase "P7  install misuse rejections"
mkdir -p "$HOME/VALIDATOR_KEYS"
touch /tmp/fake-keys.pem
assert "--squad + --count rejected"           "$MXNODE install --squad --count 4 --dry-run" 0
assert "multikey + --count rejected"          "$MXNODE install --role multikey --count 3 --dry-run" 0
assert "multikey + --with-proxy rejected"     "$MXNODE install --role multikey --with-proxy --dry-run" 0
assert "--keys-file with observer rejected"   "$MXNODE install --role observer --squad --keys-file /tmp/fake-keys.pem --dry-run" 0
assert "--keys-file with validator rejected"  "$MXNODE install --role validator --keys-file /tmp/fake-keys.pem --dry-run" 0
assert "--backup with observer rejected"      "$MXNODE install --role observer --squad --backup --dry-run" 0
# `--backup` is now allowed for validators (the wizard prompts for
# RedundancyLevel for both multikey and validator installs). Observers
# still reject because they don't sign blocks.
assert "multikey without keys file rejected"  "$MXNODE install --role multikey --dry-run" 0
rm -f /tmp/fake-keys.pem
rmdir "$HOME/VALIDATOR_KEYS" 2>/dev/null || true

# ============================================================
# P8  install dry-run shape matrix
# ============================================================
phase "P8  install dry-run shape matrix"
mkdir -p "$HOME/VALIDATOR_KEYS"
printf -- "-----BEGIN PRIVATE KEY-----\nFAKE==\n-----END PRIVATE KEY-----\n" > "$HOME/VALIDATOR_KEYS/allValidatorsKeys.pem"
assert "validator default dry-run"            "$MXNODE install --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "validator --count 2 dry-run"          "$MXNODE install --role validator --count 2 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "validator --squad dry-run"            "$MXNODE install --role validator --squad --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "observer --count 3 dry-run"           "$MXNODE install --role observer --count 3 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "observer --squad dry-run"             "$MXNODE install --role observer --squad --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "observer --squad --with-proxy dry-run" "$MXNODE install --role observer --squad --with-proxy --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --proxy-tag $PROXY_TAG --dry-run"
assert "multikey (implicit squad) dry-run"    "$MXNODE install --role multikey --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "multikey --backup dry-run"            "$MXNODE install --role multikey --backup --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "multikey --backup 2 dry-run"          "$MXNODE install --role multikey --backup 2 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
# Validator-with-backup is the new positive case after we widened the
# contract: a validator running as a backup-of-primary host stamps
# RedundancyLevel into prefs.toml.
assert "validator --backup dry-run"           "$MXNODE install --role validator --backup --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "validator --backup 3 dry-run"         "$MXNODE install --role validator --backup 3 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "multikey --squad (no-op) dry-run"     "$MXNODE install --role multikey --squad --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"
assert "--json install dry-run"               "$MXNODE --json install --role multikey --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --dry-run"

# ============================================================
# P9  REAL install — observer single, then verify + cleanup
# ============================================================
phase "P9  real install: observer single"
assert "install --role observer --count 1 (compiles binary)" \
    "$MXNODE install --role observer --count 1 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
assert "mxnode.toml exists"          "[ -f $HOME/.config/mxnode/mxnode.toml ]"
assert "node-0 workdir exists"       "[ -d $HOME/elrond-nodes/node-0/config ]"
assert "elrond-node-0.service exists" "[ -f /etc/systemd/system/elrond-node-0.service ]"
assert "binary symlinked"            "[ -L $HOME/elrond-nodes/node-0/node ]"
assert "status reads state"          "$MXNODE status >/dev/null"
assert "cleanup --yes --execute"     "$MXNODE cleanup --yes --execute >/dev/null"
assert "no leftover units"           "! ls /etc/systemd/system/elrond-*.service >/dev/null 2>&1"
assert "no leftover workdirs"        "[ ! -d $HOME/elrond-nodes ]"

# ============================================================
# P10  REAL install — observer squad with proxy + lifecycle
# ============================================================
phase "P10  real install: observer squad + proxy + lifecycle"
assert "install observer --squad --with-proxy" \
    "$MXNODE install --role observer --squad --with-proxy --binary-tag $NODE_TAG --config-tag $CONFIG_TAG --proxy-tag $PROXY_TAG"
assert "proxy unit lands"            "[ -f /etc/systemd/system/elrond-proxy.service ]"
assert "all 4 node units land"       "[ \$(ls /etc/systemd/system/elrond-node-*.service | wc -l) -eq 4 ]"

assert "start --node 0"              "$MXNODE start --node 0"
assert "start --shard metachain"     "$MXNODE start --shard metachain"
assert "start --observers-only"      "$MXNODE start --observers-only"
assert "start --validators-only rejects (no validators)" \
    "$MXNODE start --validators-only" 0
assert "start --all"                 "$MXNODE start --all"
sleep 3
assert "is-active node-0" "[ \"\$(systemctl is-active elrond-node-0.service)\" = \"active\" ]"
assert "is-active node-3" "[ \"\$(systemctl is-active elrond-node-3.service)\" = \"active\" ]"

assert "stop --node 0"               "$MXNODE stop --node 0"
assert "stop --shard 1"              "$MXNODE stop --shard 1"
assert "stop --all"                  "$MXNODE stop --all"

assert "start --all (re-bring-up)"   "$MXNODE start --all"
sleep 1
assert "restart --all (rolling)"     "$MXNODE restart --all"
assert "restart --strategy parallel" "$MXNODE restart --all --strategy parallel --max-parallel 2"
assert "restart --node 0 --node 2"   "$MXNODE restart --node 0 --node 2"

# Logs (don't spend time waiting for content)
assert "logs --node 0 --since 30s"   "$MXNODE logs --node 0 --since 30s >/dev/null"
assert "logs --node 0 --since 1h"    "$MXNODE logs --node 0 --since 1h >/dev/null"
assert "logs --node 0 --since '1 hour ago'" "$MXNODE logs --node 0 --since '1 hour ago' >/dev/null"
assert "logs combined --node 0 --node 1" "$MXNODE logs --node 0 --node 1 --since 30s >/dev/null"
assert "logs --follow + --save-archive conflict" "$MXNODE logs --node 0 --follow --save-archive" 0

# Stop everything before destructive ops
$MXNODE stop --all >/dev/null 2>&1

# DB ops
assert "db remove without --yes refuses" "$MXNODE db remove --node 0" 0
assert "db remove --yes" "$MXNODE db remove --node 0 --yes"
# db prune against synthetic Epoch_N dirs. We assert against ONLY the
# Epoch_* glob — the node also creates Static/ and friends inside db/
# that prune is not supposed to touch, so a bare `wc -l` of the dir
# would count those and fail spuriously.
for i in 0 1 2 3 4; do mkdir -p "$HOME/elrond-nodes/node-1/db/Epoch_$i"; done
assert "db prune trims to --epochs 2" "$MXNODE db prune --node 1 --epochs 2"
assert "after prune: 2 Epoch_N dirs left" \
    "[ \$(find $HOME/elrond-nodes/node-1/db -maxdepth 1 -type d -name 'Epoch_*' | wc -l) -eq 2 ]"

# add-nodes refuses on squad install
assert "add-nodes refuses on squad" "$MXNODE add-nodes --count 1 --role observer" 0

# reapply-config + override edit
assert "reapply-config (no override yet)" "$MXNODE reapply-config >/dev/null"
$MXNODE config set overrides.prefs."Preferences.Identity" "mxnode-ci" >/dev/null 2>&1 || true
# The above might fail if the dotted-path with quoted segment isn't supported;
# fall back to manual edit.
python3 -c "
p='$HOME/.config/mxnode/mxnode.toml'
b=open(p).read()
if 'overrides.prefs' not in b:
    b += '\n[overrides.prefs]\n\"Preferences.Identity\" = \"mxnode-ci\"\n'
open(p,'w').write(b)
" 2>/dev/null
assert "reapply-config propagates override" "$MXNODE reapply-config"
assert "override present in node-0 prefs" "grep -q 'mxnode-ci' $HOME/elrond-nodes/node-0/config/prefs.toml"

# upgrade dry-runs (every flag combination, no actual upgrade)
assert "upgrade --dry-run"                       "$MXNODE upgrade --binary-tag $NODE_TAG --dry-run >/dev/null"
assert "upgrade --strategy parallel --dry-run"  "$MXNODE upgrade --binary-tag $NODE_TAG --strategy parallel --max-parallel 2 --dry-run >/dev/null"
assert "upgrade --shard 0 --dry-run"            "$MXNODE upgrade --binary-tag $NODE_TAG --shard 0 --dry-run >/dev/null"
assert "upgrade --node 0 --dry-run"             "$MXNODE upgrade --binary-tag $NODE_TAG --node 0 --dry-run >/dev/null"
assert "upgrade --node + --shard mutex rejects" "$MXNODE upgrade --binary-tag $NODE_TAG --node 0 --shard metachain --dry-run" 0
assert "upgrade --skip-validators --dry-run"   "$MXNODE upgrade --binary-tag $NODE_TAG --skip-validators --dry-run >/dev/null"

# Cleanup
assert "cleanup --yes (dry-run shows plan)"    "$MXNODE cleanup --yes >/dev/null"
assert "cleanup --yes --execute"               "$MXNODE cleanup --yes --execute >/dev/null"
assert "no leftover proxy unit"                "[ ! -f /etc/systemd/system/elrond-proxy.service ]"
assert "no leftover node units"                "! ls /etc/systemd/system/elrond-node-*.service >/dev/null 2>&1"

# ============================================================
# P11  multikey variants — primary + backup + backup 2
# ============================================================
phase "P11  multikey variants"
verify_multikey() {
    local label="$1"
    local expected_redundancy="$2"
    local ok=1
    for i in 0 1 2 3; do
        local rl
        rl=$(grep -E "^   RedundancyLevel" "$HOME/elrond-nodes/node-$i/config/prefs.toml" 2>/dev/null | awk '{print $3}')
        local mode
        mode=$(stat -c '%a' "$HOME/elrond-nodes/node-$i/config/allValidatorsKeys.pem" 2>/dev/null)
        if [ "$rl" != "$expected_redundancy" ] || [ "$mode" != "600" ]; then
            ok=0
            break
        fi
    done
    if [ $ok -eq 1 ]; then
        echo "  ✓ $label"
        PASS=$((PASS+1))
    else
        echo "  ✗ $label"
        FAIL=$((FAIL+1))
        FAILED_TESTS+=("$label")
    fi
}

assert "install multikey primary"    "$MXNODE install --role multikey --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
verify_multikey "primary RedundancyLevel=0 + keys mode 0600 on all 4 nodes" 0
assert "cleanup primary"             "$MXNODE cleanup --yes --execute >/dev/null"

assert "install multikey --backup"   "$MXNODE install --role multikey --backup --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
verify_multikey "backup RedundancyLevel=1 + keys mode 0600 on all 4 nodes" 1
assert "cleanup backup"              "$MXNODE cleanup --yes --execute >/dev/null"

assert "install multikey --backup 2" "$MXNODE install --role multikey --backup 2 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
verify_multikey "backup-2 RedundancyLevel=2 + keys mode 0600 on all 4 nodes" 2
assert "cleanup backup-2"            "$MXNODE cleanup --yes --execute >/dev/null"

cleanup_artifacts

# ============================================================
# P12  validator + keys check + keygen
# ============================================================
phase "P12  validator + keys check + keygen"
assert "install --role validator"    "$MXNODE install --role validator --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
assert "keys check (no zip) errors"  "$MXNODE keys check" 0
mkdir -p "$HOME/VALIDATOR_KEYS"
echo "fake" > "$HOME/VALIDATOR_KEYS/node-0.zip"
assert "keys check passes after zip drop" "$MXNODE keys check"
assert "keygen default"              "$MXNODE keygen >/dev/null"
assert "keygen --output /tmp/...."    "$MXNODE keygen --for 5 --output /tmp/mxnode-test-keys >/dev/null"
assert "keygen output produced .pem" "ls /tmp/mxnode-test-keys/*.pem >/dev/null 2>&1"
assert "cleanup validator"           "$MXNODE cleanup --yes --execute >/dev/null"
cleanup_artifacts

# ============================================================
# P13  cleanup variations
# ============================================================
phase "P13  cleanup variations (--keep-binaries, --keep-config)"
assert "install (cached binary)"     "$MXNODE install --role observer --count 1 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
assert "cleanup --keep-binaries"     "$MXNODE cleanup --yes --execute --keep-binaries >/dev/null"
assert "binstore preserved"          "[ -d $HOME/mxnode/binaries ]"
assert "state removed"               "[ ! -d $HOME/.local/state/mxnode ]"
assert "config removed"              "[ ! -f $HOME/.config/mxnode/mxnode.toml ]"

assert "install (re-uses cached binary, fast)" \
    "$MXNODE install --role observer --count 1 --binary-tag $NODE_TAG --config-tag $CONFIG_TAG"
assert "cleanup --keep-config"       "$MXNODE cleanup --yes --execute --keep-config >/dev/null"
assert "config preserved"            "[ -f $HOME/.config/mxnode/mxnode.toml ]"
assert "binstore removed"            "[ ! -d $HOME/mxnode/binaries ]"

# Final pristine
assert "full cleanup"                "$MXNODE cleanup --yes --execute >/dev/null"
assert "host pristine"               "[ ! -e $HOME/.config/mxnode ] && [ ! -e $HOME/.local/state/mxnode ] && [ ! -e $HOME/mxnode ] && [ ! -e $HOME/elrond-nodes ]"

# ============================================================
# Summary
# ============================================================
echo
echo "============================================================"
echo "  Sweep complete"
echo "============================================================"
echo "  passed:  $PASS"
echo "  failed:  $FAIL"
if [ $FAIL -gt 0 ]; then
    echo
    echo "  FAILED tests:"
    for t in "${FAILED_TESTS[@]}"; do echo "    - $t"; done
    exit 1
fi
exit 0
