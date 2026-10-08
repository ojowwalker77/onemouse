# onemouse protocol (v3)

Source of truth: [`crates/onemouse-protocol`](../crates/onemouse-protocol/src/lib.rs). This page describes the flow.

## Roles

Two independent pairs of roles:

- **Server / client** (the connection, fixed): the **server** (macOS) listens on TCP `24801`; the **client** (Windows) connects. Pairing, `--host` and autostart all follow from this and don't change when the user switches machines.
- **Main / remote** (the input, a user setting): the **main** side has the keyboard and mouse the user is using. It captures input, decides when the cursor crosses an edge, translates shortcuts for the other OS and sends the result. The **remote** side injects what it receives. Either machine can be main (`Main::Server` or `Main::Client`), and the user can switch at any time (`SetMain`).

## Transport

The TCP connection is encrypted and mutually authenticated by [`crates/onemouse-transport`](../crates/onemouse-transport/src/lib.rs) (wire format in its crate docs):

1. Noise `XX_25519_ChaChaPoly_BLAKE2s` handshake; the client initiates; device names travel in the handshake.
2. Trust exchange: pinned keys continue silently; a name pinned to a different key closes the connection (never re-paired automatically); otherwise **pairing**.
3. Pairing: both screens show a 6-digit code derived from the handshake hash **and two random nonces exchanged commit-then-reveal** (the initiator commits to its nonce before seeing the responder's, so nobody, not even a man in the middle running two handshakes, can steer the code). Both users confirm it matches, then each side pins the other's key. The server only pairs while its user has pairing mode open, one pairing at a time. 60 s limit.
4. Everything below (framing, messages) runs unchanged inside encrypted records.

Discovery: the server advertises `_onemouse._tcp` over mDNS with TXT `fp=<key fingerprint>`, `name=`, `v=<PROTOCOL_VERSION>`.

## Framing

Each message is a `u32` little-endian byte length, followed by the [postcard](https://docs.rs/postcard) encoding of `Message`. Frames over 1 MiB are a protocol error.

Use `write_message` / `read_message` (or `encode` / `decode` with async I/O).

## Conversation

```
client (PC)                               server (Mac)
    │── Hello{version,name,os,displays} ─────▶│
    │◀─ Welcome{version,name,os,displays,main} │   or Reject
    │◀──────────── Arrangement{x,y} ───────────│   now and whenever the user rearranges
    │◀──── DisplaysChanged ───▶│ either side, whenever its displays change
    │◀──────── SetMain{main} ─────────▶│ either side, when the user switches
    │◀───────────── Ping ⇄ Pong ──────────────▶│ either side, every 2 s; drop after 6 s of silence

main ──▶ remote (whichever way round `main` says):
    Enter{x,y} · MouseMove · MouseButton · Scroll · Key{code} · Leave
```

## Semantics

- **Coordinates:** in the **remote** side's own global desktop coordinates, in the units of its `Display`s (Windows: physical pixels; macOS: points). Can be negative. The main side tracks the cursor and always sends absolute positions.
- **Arrangement:** the client's desktop origin `(0, 0)` sits at `(x, y)` in the server's desktop coordinates (server units). The server owns it (the Arrange UI lives there) and sends it after `Welcome` and on every change, so whichever side is main can run edge detection.
- **SetMain:** the receiver adopts and persists the new setting and doesn't echo it. A side that stops being main while the cursor is on the remote side sends `Leave` first. At connect, `Welcome.main` (the server's setting) wins.
- **Enter:** the remote side moves the cursor to `(x, y)` and assumes nothing is held. If the user is still holding a key, the main side sends its `Key{pressed: true}` right after `Enter`.
- **Leave / disconnect / any error:** the remote side releases every key and button it still holds.
- **Keys:** USB HID usages, page 0x07 ([`key.rs`](../crates/onemouse-protocol/src/key.rs)), **already translated** for the remote side's OS by the main side (shared engine in `onemouse-core`): e.g. Mac main → PC: Cmd→Ctrl, Option→Alt, Ctrl→Win, Cmd+Tab→Alt+Tab; PC main → Mac: the same table backwards.
- **Scroll:** `120` units = one notch. Smaller values are high-resolution. `dy > 0` scrolls up, `dx > 0` scrolls right.

## Changing the protocol

Variants and fields are **append-only** (new variants at the end of `Message`, new fields at the end of a variant), and `Hello` stays variant 0 with `protocol_version` first. Any change to the encoding bumps `PROTOCOL_VERSION`, updates the `wire_layout_is_stable` test, and needs a 👍 from the other side's agent in the PR (label `protocol`).

## Security

Since v2 every byte after the handshake is encrypted and authenticated (ChaCha20-Poly1305, implicit nonces: tampered, replayed or reordered records fail). Each device's static X25519 key lives in its per-user config directory with user-only permissions, and peers are pinned by key after a pairing both users confirmed. What's still trusted:

- The pairing moment: users must actually compare the codes. A wrong "yes" pins an attacker.
- The local machines: anything running as the user can read the key files and inject input anyway.
- Availability: anyone on the network can still connect and fail, or interfere with traffic.

v1 (plaintext, M1) is gone: a v1 peer fails the handshake.
