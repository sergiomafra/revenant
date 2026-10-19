# Revenant

Revenant is a free, open-source app for Mac and Windows that recovers photos and videos from SD cards and external drives,
including ones that were corrupted, formatted or overwritten by mistake. It aims at paid-tool recovery quality, with a clear
preview of what can be recovered before you commit, and it never writes to the card or drive it reads from.

## Status

Under construction: the recovery engine is being built. There is no app to download yet.

## License

[GPL-3.0-only](LICENSE), copyright Sérgio Mafra.

## Contributing

Issues and pull requests are welcome. Everything in the repository (code, comments, commits, docs) is in English. Before
opening a pull request, run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace`.
Dependencies must be under permissive licenses (see `deny.toml`).
