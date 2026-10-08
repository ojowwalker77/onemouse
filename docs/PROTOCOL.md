# onemouse protocol (v1)

Source of truth: [`crates/onemouse-protocol`](../crates/onemouse-protocol/src/lib.rs). This page describes the flow.

## Roles

- **Primary** (macOS): owns the keyboard and trackpad. Listens on TCP `24801`.
- **Secondary** (Windows): connects to the primary and injects the input it receives.

## Framing

Each message is a `u32` little-endian byte length, followed by the [postcard](https://docs.rs/postcard) encoding of `Message`. Frames over 1 MiB are a protocol error.

Use `write_message` / `read_message` (or `encode` / `decode` with async I/O).

## Conversation

```
secondary                         primary
    │── Hello{version,name,os,displays} ──▶│
    │◀───────── Welcome / Reject ───────────│
    │                                       │   cursor crosses the shared edge
    │◀────────────── Enter{x,y} ────────────│
    │◀── MouseMove / MouseButton / Scroll ──│
    │◀────────────── Key{code} ─────────────│
    │◀──────────────── Leave ───────────────│   cursor crosses back
    │── DisplaysChanged{displays} ─────────▶│   any time
    │◀──────────── Ping ⇄ Pong ────────────▶│   either side, every 2 s; drop after 6 s of silence
```

## Semantics

- **Coordinates:** secondary virtual-desktop **physical pixels**. Can be negative. The primary tracks the cursor and always sends absolute positions.
- **Enter:** the secondary moves the cursor to `(x, y)` and assumes nothing is held. If the user is still holding a key, the primary sends its `Key{pressed: true}` right after `Enter`.
- **Leave / disconnect / any error:** the secondary releases every key and button it still holds.
- **Keys:** USB HID usages, page 0x07 ([`key.rs`](../crates/onemouse-protocol/src/key.rs)). Modifier remapping happens on the primary. Default: Cmd→Ctrl, Ctrl→Win, Option→Alt.
- **Scroll:** `120` units = one notch. Smaller values are high-resolution. `dy > 0` scrolls up, `dx > 0` scrolls right.

## Changing the protocol

Variants and fields are **append-only**, and `Hello` stays variant 0. Any change to the encoding bumps `PROTOCOL_VERSION`, updates the `wire_layout_is_stable` test, and needs a 👍 from the other side's agent in the PR (label `protocol`).

## Security

v1 is **plaintext**: dev use on a trusted LAN only. M2 wraps the TCP stream in Noise `XX` with pinned keys and one-time pairing, and keeps the framing above unchanged inside the encrypted channel.
