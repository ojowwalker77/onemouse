# onemouse protocol (v2)

Source of truth: [`crates/onemouse-protocol`](../crates/onemouse-protocol/src/lib.rs). This page describes the flow.

## Roles

- **Primary** (macOS): owns the keyboard and trackpad. Listens on TCP `24801`.
- **Secondary** (Windows): connects to the primary and injects the input it receives.

## Transport

The TCP connection is encrypted and mutually authenticated by [`crates/onemouse-transport`](../crates/onemouse-transport/src/lib.rs) (wire format in its crate docs):

1. Noise `XX_25519_ChaChaPoly_BLAKE2s` handshake; the secondary initiates; device names travel in the handshake.
2. Trust exchange: pinned keys continue silently; a name pinned to a different key closes the connection (never re-paired automatically); otherwise **pairing**.
3. Pairing: both screens show a 6-digit code derived from the handshake hash, both users confirm it matches, then each side pins the other's key. The primary only pairs while its user has pairing mode open. 60 s limit.
4. Everything below (framing, messages) runs unchanged inside encrypted records.

Discovery: the primary advertises `_onemouse._tcp` over mDNS with TXT `fp=<key fingerprint>`, `name=`, `v=<PROTOCOL_VERSION>`.

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

Since v2 every byte after the handshake is encrypted and authenticated (ChaCha20-Poly1305, implicit nonces: tampered, replayed or reordered records fail). Each device's static X25519 key lives in its per-user config directory with user-only permissions, and peers are pinned by key after a pairing both users confirmed. What's still trusted:

- The pairing moment: users must actually compare the codes. A wrong "yes" pins an attacker.
- The local machines: anything running as the user can read the key files and inject input anyway.
- Availability: anyone on the network can still connect and fail, or interfere with traffic.

v1 (plaintext, M1) is gone: a v1 peer fails the handshake.
