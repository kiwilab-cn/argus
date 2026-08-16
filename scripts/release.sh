#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
ENV_FILE="${PROJECT_DIR}/.env"

if [[ ! -f "${ENV_FILE}" ]]; then
  echo "missing ${ENV_FILE}; copy .env.example and fill in the secrets" >&2
  exit 1
fi

if ! grep -Eq '^GETLLM_API_KEY=.+$' "${ENV_FILE}"; then
  echo "GETLLM_API_KEY is missing in .env" >&2
  exit 1
fi

if ! grep -Eq '^FEISHU_WEBHOOK_URL=https://.+$' "${ENV_FILE}"; then
  echo "FEISHU_WEBHOOK_URL is missing or is not HTTPS in .env" >&2
  exit 1
fi

cd "${PROJECT_DIR}"
docker compose --env-file "${ENV_FILE}" config --quiet
docker compose --env-file "${ENV_FILE}" build --pull
docker compose --env-file "${ENV_FILE}" up -d --remove-orphans
docker compose --env-file "${ENV_FILE}" ps
