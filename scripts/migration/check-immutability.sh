#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

duplicate_versions="$(
    find crates/aionui-db/migrations -maxdepth 1 -type f -name '*.sql' -print \
        | awk -F/ '
            {
                name = $NF
                if (name ~ /^[0-9]+_/) {
                    version = name
                    sub(/_.*/, "", version)
                    version += 0
                    count[version]++
                    files[version] = files[version] (files[version] == "" ? "" : ", ") name
                }
            }
            END {
                for (version in count) {
                    if (count[version] > 1) {
                        print version ": " files[version]
                    }
                }
            }
        ' \
        | sort
)"

if [[ -n "$duplicate_versions" ]]; then
    cat >&2 <<'EOF'
Duplicate database migration versions are not allowed.

Rename the later migration to the next unused numeric prefix.

Duplicate versions:
EOF
    echo "$duplicate_versions" >&2
    exit 1
fi

if [[ "${AIONCORE_ALLOW_MAIN_MIGRATION_EDIT:-}" == "1" ]]; then
    echo "AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1; skipping migration immutability check"
    exit 0
fi

base_ref="${AIONCORE_MIGRATION_BASE_REF:-}"
if [[ -z "$base_ref" ]]; then
    main_upstream="$(git rev-parse --abbrev-ref --symbolic-full-name 'main@{upstream}' 2>/dev/null || true)"
    if [[ -n "$main_upstream" ]] && git rev-parse --verify --quiet "$main_upstream" >/dev/null; then
        base_ref="$main_upstream"
    elif git rev-parse --verify --quiet origin/main >/dev/null; then
        base_ref="origin/main"
    elif git rev-parse --verify --quiet main >/dev/null; then
        base_ref="main"
    else
        echo "No configured main upstream, origin/main, or main ref found; skipping migration immutability check"
        exit 0
    fi
fi

if ! git rev-parse --verify --quiet "$base_ref" >/dev/null; then
    echo "Migration immutability base ref not found: $base_ref" >&2
    exit 1
fi

base_commit="$(git merge-base HEAD "$base_ref")"
added_migrations="$(
    {
        git diff --name-only --diff-filter=A "$base_commit" -- 'crates/aionui-db/migrations/*.sql'
        git ls-files --others --exclude-standard -- 'crates/aionui-db/migrations/*.sql'
    } | sort -u
)"
invalid_new_migrations=""
while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    name="${path##*/}"
    if [[ ! "$name" =~ ^20[0-9]{12}_[a-z0-9]+(_[a-z0-9]+)*[.]sql$ ]]; then
        invalid_new_migrations+="${invalid_new_migrations:+$'\n'}$path"
    fi
done <<< "$added_migrations"

if [[ -n "$invalid_new_migrations" ]]; then
    cat >&2 <<'EOF'
New database migrations must use a 14-digit UTC timestamp prefix.

Create new files as YYYYMMDDHHMMSS_descriptive_name.sql. Existing shipped migrations keep their original names.

Invalid new migrations:
EOF
    echo "$invalid_new_migrations" >&2
    exit 1
fi

changed="$(
    git diff --name-status --diff-filter=DMR "$base_commit" -- 'crates/aionui-db/migrations/*.sql'
)"

if [[ -n "$changed" ]]; then
    cat >&2 <<'EOF'
Existing migration files from main must not be modified or deleted.

Fix this by reverting changes to existing migration files and adding a new UTC timestamp-prefixed migration instead.
If this is an intentional high-risk exception, rerun with AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1.

Changed existing migrations:
EOF
    echo "$changed" >&2
    exit 1
fi

echo "Migration immutability check passed"
