# MnemeCache Client Libraries

The official Rust client library is **Pontus** (`mneme-client/` crate in this workspace).

For building clients in other languages, see the full wire protocol specification:
**[docs/CLIENT_PROTOCOL.md](../docs/CLIENT_PROTOCOL.md)**

The protocol document covers everything needed to implement a compatible client:
frame format, command IDs, payload schemas, authentication, multiplexing,
consistency levels, leader redirect (HA), error codes, and connection lifecycle.

Pontus (`mneme-client/src/`) serves as the reference implementation.
