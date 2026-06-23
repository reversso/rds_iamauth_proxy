#!/usr/bin/env bash
set -eu

# Check environment
if [ -z "${DB_HOST:-}" ]; then
  echo "ERROR: DB_HOST is not set"
  exit 1
fi

# Build docker image if '--build' is passed
if [ "${1:-}" = "--build" ]; then
  docker build -t rds_proxy:latest .
  shift
fi

# Run docker container with AWS credentials and proxy settings
docker run --rm -it -u "$(id -u):$(id -g)" \
  -p 5435:5435 \
  -v $HOME/.aws:/home/rdsproxy/.aws \
  -e AWS_PROFILE="${AWS_PROFILE:-}" \
  -e AWS_REGION="${AWS_REGION:-}" \
  -e AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-}" \
  -e DB_HOST="${DB_HOST}" \
  -e DB_PORT="${DB_PORT:-}" \
  -e CONNECT_HOST="${CONNECT_HOST:-}" \
  -e CONNECT_PORT="${CONNECT_PORT:-}" \
  -e LISTEN_ADDR="${LISTEN_ADDR:-0.0.0.0:5435}" \
  -e PASSWORD_CACHE_TTL_SECS="${PASSWORD_CACHE_TTL_SECS:-}" \
  -e HOME=/home/rdsproxy \
  rds_proxy:latest "${@}"
