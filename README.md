# coqui-proxy

[![Version](https://img.shields.io/crates/v/coqui-proxy.svg)](https://crates.io/crates/coqui-proxy)
[![Rust version](https://img.shields.io/crates/msrv/coqui-proxy.svg)](https://crates.io/crates/coqui-proxy)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/Boriken-Dev/coqui-proxy/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/coqui-proxy)](https://docs.rs/coqui-proxy)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/coqui-proxy/test.yaml)](https://github.com/Boriken-Dev/coqui-proxy/actions/workflows/test.yaml)

**Which proxy should this request use, and connect me through it.** Which proxy a request should use, and a connection through it: the system's proxy settings, PAC and WPAD, bypass rules, proxy login and CONNECT/SOCKS connectors.

## Features

- Under construction.

## Installation

```sh
cargo add --git https://github.com/Boriken-Dev/coqui-proxy coqui-proxy
```

| Feature | Adds | Default |
| --- | --- | --- |

## Quick start

Under construction.

## API overview

See [AGENTS.md](https://github.com/Boriken-Dev/coqui-proxy/blob/main/AGENTS.md).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

### Releasing

No release is cut yet; the crate is consumed by git revision.

## License

MIT OR Apache-2.0, at your option. See [LICENSE](https://github.com/Boriken-Dev/coqui-proxy/blob/main/LICENSE).
