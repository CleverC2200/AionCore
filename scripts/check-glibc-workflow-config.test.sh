#!/usr/bin/env bash
set -euo pipefail

workflows=(
  ".github/workflows/release.yml"
  ".github/workflows/build-manual.yml"
)

arm64_cross_image="$(sed -nE 's/^[[:space:]]*image[[:space:]]*=[[:space:]]*"([^"]+)"[[:space:]]*$/\1/p' Cross.toml | head -n 1)"
if [[ ! "${arm64_cross_image}" =~ ^ghcr[.]io/cross-rs/aarch64-unknown-linux-gnu@sha256:[a-f0-9]{64}$ ]]; then
  echo "Cross.toml must pin Linux ARM64 to an immutable ghcr.io/cross-rs image digest" >&2
  exit 1
fi

grep -Fq "${arm64_cross_image}" Cross.toml \
  || {
    echo "Cross.toml must contain the resolved Linux ARM64 cross image digest" >&2
    exit 1
  }

grep -Fq 'target: x86_64-unknown-linux-gnu' ".github/workflows/release.yml" \
  && grep -Fq 'os: ubuntu-22.04' ".github/workflows/release.yml" \
  || {
    echo ".github/workflows/release.yml must keep Linux x64 on ubuntu-22.04" >&2
    exit 1
  }

grep -Fq '"platform":"linux-x64","os":"ubuntu-22.04","target":"x86_64-unknown-linux-gnu"' ".github/workflows/build-manual.yml" \
  || {
    echo ".github/workflows/build-manual.yml must keep Linux x64 on ubuntu-22.04" >&2
    exit 1
  }

arm64_cross_rev=""
for workflow in "${workflows[@]}"; do
  if [[ ! -f "${workflow}" ]]; then
    echo "Workflow not found: ${workflow}" >&2
    exit 1
  fi

  grep -Fq 'LINUX_X64_GLIBC_MAX: "GLIBC_2.34"' "${workflow}" \
    || {
      echo "${workflow} must pin the Linux x64 GLIBC ceiling to GLIBC_2.34" >&2
      exit 1
    }

  workflow_cross_rev="$(sed -nE 's/^[[:space:]]*CROSS_GIT_REV:[[:space:]]*"([a-f0-9]+)"[[:space:]]*$/\1/p' "${workflow}" | head -n 1)"
  if [[ ! "${workflow_cross_rev}" =~ ^[a-f0-9]{40}$ ]]; then
    echo "${workflow} must pin CROSS_GIT_REV to an immutable 40-character Git revision" >&2
    exit 1
  fi
  if [[ -z "${arm64_cross_rev}" ]]; then
    arm64_cross_rev="${workflow_cross_rev}"
  elif [[ "${workflow_cross_rev}" != "${arm64_cross_rev}" ]]; then
    echo "${workflow} must use the same CROSS_GIT_REV as the other release workflows" >&2
    exit 1
  fi

  grep -Fq 'cargo install cross --git https://github.com/cross-rs/cross --rev "${CROSS_GIT_REV}" --locked' "${workflow}" \
    || {
      echo "${workflow} must install cross from the pinned git revision" >&2
      exit 1
    }

  grep -Fq "docker pull ${arm64_cross_image}" "${workflow}" \
    || {
      echo "${workflow} must pre-pull the pinned Linux ARM64 cross image" >&2
      exit 1
    }

  grep -Fq "matrix.target == 'x86_64-unknown-linux-gnu'" "${workflow}" \
    || {
      echo "${workflow} must verify the Linux x64 GLIBC baseline" >&2
      exit 1
    }

  grep -Fq '${LINUX_X64_GLIBC_MAX}' "${workflow}" \
    || {
      echo "${workflow} must pass LINUX_X64_GLIBC_MAX to the GLIBC checker" >&2
      exit 1
    }
done

echo "Linux build inputs are immutable and consistent; GLIBC gates are configured for x64 and arm64"
