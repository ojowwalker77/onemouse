<img src="assets/icon.svg" width="64" height="64" alt="">

# onemouse

Use your Mac's keyboard and trackpad on a Windows PC. Put the PC's screen next to the Mac's, push the cursor across the edge, and keep typing.

> Early development. Tracking issue: [#1](https://github.com/ojowwalker77/onemouse/issues/1).

| Crate | Runs on | What it does |
|---|---|---|
| [`onemouse-protocol`](crates/onemouse-protocol) | both | Shared wire format ([docs](docs/PROTOCOL.md)) |
| [`onemouse-mac`](crates/onemouse-mac) | macOS | Captures input and switches screens at the edge |
| [`onemouse-win`](crates/onemouse-win) | Windows | Receives and injects input |

```sh
cargo test
```

## Try it (dev, plaintext, trusted LAN only)

On the Mac (grant your terminal **Accessibility** and **Input Monitoring** when asked):

```sh
cargo run --release -p onemouse-mac -- --side right   # where the PC sits: left/right/top/bottom
```

On the PC:

```sh
cargo run --release -p onemouse-win -- --host <mac-ip>
```

Push the cursor past that edge of the Mac to control the PC, and push it back to return. **Ctrl+Option+Cmd+Esc** always brings it home.

To test without a PC, run `cargo run -p onemouse-win -- --dry-run --host 127.0.0.1`, or run `cargo run -p onemouse-mac --example simulate` for a scripted session that doesn't touch your input.
