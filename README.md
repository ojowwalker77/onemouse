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

## Try it

On the Mac (grant your terminal **Accessibility** and **Input Monitoring** when asked):

```sh
cargo run --release -p onemouse-mac -- --arrange   # then drag the PC to where it sits
```

On the PC:

```sh
cargo run --release -p onemouse-win            # finds the Mac via mDNS (or --host <mac-ip>)
```

**First time only:** choose **Pair a New PC…** in the Mac's ⇄ menu. Both screens show a 6-digit code. Confirm on the Mac, and type `y` on the PC, only if the codes match. After that the PC reconnects silently. `onemouse-win --peers` lists pairings, and `--forget <name>` removes one (for example after reinstalling the Mac).

The ⇄ menu-bar item also opens **Arrange Displays**: drag the PC next to your Mac screens, the way System Settings arranges monitors. Push the cursor where they touch to control the PC, and push it back to return. **Ctrl+Option+Cmd+Esc** always brings it home.

Everything is encrypted (Noise XX) between devices that paired once; see [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

To test without a PC, run `cargo run -p onemouse-win -- --dry-run` (it keeps its own key under `…/onemouse/dry-run`, so it pairs separately from the real app), or run `cargo run -p onemouse-mac --example simulate` for a scripted session that doesn't touch your input.
