#!/usr/bin/env bash
#
# Put a xous-core checkout at vendor/xous-core, which is where every path
# dependency in Cargo.toml points and where build.sh looks for the host tools.
#
# Three ways to get one, tried in order:
#
#   1. $XOUS_CORE points at a checkout   -> symlink it
#   2. a sibling clone is lying around   -> symlink it
#   3. neither                           -> shallow, blobless clone of the pin
#
# The symlink cases exist because a full xous-core working tree is a couple of
# gigabytes; if you already have one there is no reason for a second. The clone
# case is what CI uses.
#
# Idempotent: run it as often as you like. It only reports and repairs.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PIN_FILE="${HERE}/xous-core.pin"
DEST="${HERE}/vendor/xous-core"

# Parse `key = value` out of the pin file, ignoring comments.
pin_get() { sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" "${PIN_FILE}" | head -1; }
URL="$(pin_get url)"
COMMIT="$(pin_get commit)"
[[ -n "${URL}" && -n "${COMMIT}" ]] || { echo "bootstrap: cannot read url/commit from ${PIN_FILE}" >&2; exit 1; }

say() { printf '%s\n' "$*"; }

# A checkout is usable if the crates we actually name are present. Checking for
# files rather than for a .git means a tarball or a symlink into a worktree works.
usable() {
    local d="$1"
    [[ -f "${d}/libs/bao1x-hal/Cargo.toml" \
    && -f "${d}/libs/bao1x-api/Cargo.toml" \
    && -f "${d}/libs/ux-api/Cargo.toml" \
    && -f "${d}/utralib/Cargo.toml" \
    && -f "${d}/svd2utra/Cargo.toml" \
    && -f "${d}/tools/Cargo.toml" \
    && -f "${d}/devkey/dev.key" ]]
}

report_commit() {
    local d="$1" at
    at="$(git -C "${d}" rev-parse HEAD 2>/dev/null || echo unknown)"
    if [[ "${at}" == "${COMMIT}" ]]; then
        say "    at the pinned commit ${COMMIT:0:12}"
    elif [[ "${at}" == unknown ]]; then
        say "    (not a git checkout -- cannot compare against the pin)"
    else
        say "    ⚠ at ${at:0:12}, pin says ${COMMIT:0:12}"
        say "      Fine for day-to-day work; CI builds the pin. Bump xous-core.pin"
        say "      if you mean to move BadgyOS onto this commit."
    fi
}

if [[ -e "${DEST}" ]]; then
    if usable "${DEST}"; then
        if [[ -L "${DEST}" ]]; then
            say "==> vendor/xous-core -> $(readlink "${DEST}")"
        else
            say "==> vendor/xous-core present"
        fi
        report_commit "${DEST}"
        exit 0
    fi
    say "==> vendor/xous-core exists but is missing crates BadgyOS needs"
    if [[ -L "${DEST}" ]]; then
        say "    it is a symlink to $(readlink "${DEST}") -- repoint or remove it"
    else
        say "    remove it and re-run, or check out the pin inside it"
    fi
    exit 1
fi

mkdir -p "${HERE}/vendor"

# 1 and 2: adopt an existing checkout by symlink.
CANDIDATES=()
[[ -n "${XOUS_CORE:-}" ]] && CANDIDATES+=("${XOUS_CORE}")
CANDIDATES+=(
    "${HERE}/../baoom/upstream/xous-core"   # where this repo was born
    "${HERE}/../xous-core"
    "${HERE}/../upstream/xous-core"
    "${HOME}/Documents/baoom/upstream/xous-core"
)
for c in "${CANDIDATES[@]}"; do
    [[ -d "${c}" ]] || continue
    if usable "${c}"; then
        abs="$(cd "${c}" && pwd)"
        ln -s "${abs}" "${DEST}"
        say "==> symlinked vendor/xous-core -> ${abs}"
        [[ -n "${XOUS_CORE:-}" && "${c}" == "${XOUS_CORE}" ]] && say "    (from \$XOUS_CORE)"
        report_commit "${DEST}"
        say
        say "    Nothing in BadgyOS writes to it. If you would rather have a"
        say "    private copy, remove the symlink and re-run with XOUS_CORE unset."
        exit 0
    fi
    say "==> ${c} exists but does not look like a xous-core checkout; skipping"
done

# 3: clone the pin. Blobless and single-commit -- we need one tree, not history.
say "==> cloning ${URL} at ${COMMIT:0:12}"
say "    (blobless, single commit; set \$XOUS_CORE to reuse a checkout instead)"
git init -q "${DEST}"
git -C "${DEST}" remote add origin "${URL}"
git -C "${DEST}" -c protocol.version=2 fetch -q --depth 1 --filter=blob:none origin "${COMMIT}"
git -C "${DEST}" checkout -q FETCH_HEAD

if ! usable "${DEST}"; then
    say "bootstrap: clone finished but the crates BadgyOS needs are missing." >&2
    say "           Did the pinned commit predate them?" >&2
    exit 1
fi
say "==> vendor/xous-core ready at ${COMMIT:0:12}"
