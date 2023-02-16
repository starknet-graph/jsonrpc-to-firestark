# jsonrpc-to-firestark

Extract logs expected by [firehose-starknet](https://github.com/starknet-graph/firehose-starknet) from a trusted JSON-RPC source.

This tool exists because it could be slow to sync a Starknet node from scratch, which is currently the only way to bootstrap a [firehose-starknet](https://github.com/starknet-graph/firehose-starknet) deployment. This tool makes calls to the JSON-RPC endpoint offered by a trusted synced node, and emits the exact same format as an instrumented client node would do.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
