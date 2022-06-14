# Intro

This is a Telegram bot frontend for the [Pandoc](pandoc.org) document converter.


# Usage

Run the bot executable directly without any arguments.

Configuration is done via environment variables.

- `TELOXIDE_TOKEN`: The Telegram bot token.
- `RUST_LOG`: For [`pretty_env_logger`](https://lib.rs/crates/pretty_env_logger).
  - Recommended value: `pandoc_bot=info`
- `STATE_PATH`: Path to persistent state.
- `INPUT_BASE_PATH`: Path to temporary input files.


# Docker Image

There's a workflow that automatically builds and pushes the latest code from
`master` branch to
[kotatsuyaki/pandoc-bot](https://hub.docker.com/repository/docker/kotatsuyaki/pandoc-bot).
