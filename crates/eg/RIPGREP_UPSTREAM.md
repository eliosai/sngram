# ripgrep upstream

Source: https://github.com/BurntSushi/ripgrep

Commit: `48b0c795f4feb37343b2832d991c5c6a3900c08a`

Version: `15.1.0`

Copied paths:

- `crates/core/main.rs`
- `crates/core/haystack.rs`
- `crates/core/search.rs`
- `crates/core/logger.rs`
- `crates/core/messages.rs`
- `crates/core/flags/**`

Imported crates:

- `grep`
- `ignore`
- `bstr`
- `lexopt`
- `termcolor`
- `textwrap`
- `serde_json`
- `anyhow`
- `log`

Local patch policy:

- Keep copied code close to upstream.
- Import public crates instead of copying them.
- Put eg index code under `src/index`.
- Patch copied files only for CLI flags, dispatch, and metadata.
