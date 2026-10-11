# Contributing to Talus

Thanks for helping! Issues, ideas and pull requests are all welcome.

## Licensing of contributions

Talus is released under the [MIT License](LICENSE). By opening a pull request you agree that your
contribution is licensed under the same terms (inbound = outbound). There is no CLA to sign.

Please contribute only code you wrote yourself or that you are allowed to submit under the MIT
License, and do not copy code, assets, icons or text from proprietary database tools.

## Getting started

Requirements: macOS 12 or later, Rust 1.90 or later. Xcode is not needed.

```sh
git clone https://github.com/fallowlone/Talus.git && cd Talus
cargo run -p talus-app --release
```

## Before you open a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                    # SQLite + headless UI tests
eval "$(scripts/test-env.sh)" && cargo test --workspace   # + PostgreSQL, MySQL, Redis, MongoDB, SSH (Docker)
```

SQL Server tests are opt-in because running its container means accepting Microsoft's EULA:
`eval "$(MSSQL_ACCEPT_EULA=Y scripts/test-env.sh)"`.

Guidelines:

1. Add a test that fails without your change. UI behaviour belongs in `crates/talus-app/tests`
   (headless, driven through AccessKit); engine behaviour in `crates/talus-core/tests`.
2. Keep `docs/feature-matrix.md` and `docs/progress.md` in step with what you changed.
3. If you add, remove or update a dependency, regenerate the license list:
   `python3 scripts/third-party-licenses.py`.
4. Keep pull requests focused: one feature or fix per PR, with a short description of why.

## Reporting bugs and security issues

Use GitHub issues for bugs and feature requests. Report security problems privately as described
in [SECURITY.md](SECURITY.md).
