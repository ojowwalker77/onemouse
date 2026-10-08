<img src="assets/icon.svg" width="64" height="64" alt="">

# onemouse

Use your Mac's keyboard and trackpad on a Windows PC. Put the PC's screen next to the Mac's, push the cursor across the edge, and keep typing.

> Early development. Tracking issue: [#1](https://github.com/ojowwalker77/onemouse/issues/1).

| Crate | Runs on | What it does |
|---|---|---|
| [`onemouse-protocol`](crates/onemouse-protocol) | both | Shared wire format ([docs](docs/PROTOCOL.md)) |
| `onemouse-mac` | macOS | Captures input, edge switching, arrange UI *(coming)* |
| `onemouse-win` | Windows | Receives and injects input *(coming)* |

```sh
cargo test
```
