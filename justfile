set shell := ["bash", "-euo", "pipefail", "-c"]

# Override per call: PI_HOST=pi@192.168.1.50 just deploy
PI_HOST := env("PI_HOST", "pi@raspberrypi.local")
PI_DIR := env("PI_DIR", "~/dtek-bot")
IMAGE := "dtek-bot:latest"
TAR := "dtek-bot.tar"

default:
    @just --list

# Run locally with config.toml and .env; state goes to ./data
run:
    set -a; [ -f .env ] && . ./.env; set +a; \
    STATE_PATH="${STATE_PATH:-./data/state.json}" cargo run -- --config config.toml

# Format check, lints and tests
test:
    cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# Build the arm64 image for Raspberry Pi into dtek-bot.tar
build-pi:
    docker buildx build --platform linux/arm64 -t {{IMAGE}} --output type=docker,dest={{TAR}} .

# Build an amd64 image and load it into the local Docker
build-local:
    docker buildx build --platform linux/amd64 -t {{IMAGE}} --load .

# Build, copy to the Pi and (re)start the bot there
deploy: _check-files build-pi
    ssh {{PI_HOST}} 'mkdir -p {{PI_DIR}}/data'
    scp {{TAR}} docker-compose.yml config.toml .env {{PI_HOST}}:{{PI_DIR}}/
    ssh {{PI_HOST}} 'cd {{PI_DIR}} && chmod 600 .env && docker load -i {{TAR}} && rm -f {{TAR}} && docker compose up -d --force-recreate && docker image prune -f >/dev/null'

# Follow the bot logs on the Pi
logs:
    ssh {{PI_HOST}} 'cd {{PI_DIR}} && docker compose logs -f --tail=100'

# Bot status on the Pi
status:
    ssh {{PI_HOST}} 'cd {{PI_DIR}} && docker compose ps'

_check-files:
    @for f in config.toml .env; do [ -f "$f" ] || { echo "missing $f: copy config.example.toml / .env.example and fill it in" >&2; exit 1; }; done
