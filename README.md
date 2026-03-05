# slmodem-asterisk-bridge

Rust bridge service that connects `slmodemd` audio socket traffic to Asterisk using ARI and External Media over WebSocket.

## Current behavior
- Runs as `slmodemd -e` external helper with args `<dial-string> <socket-fd>`.
- Uses ARI to create and control:
  - outbound channel (`/channels/create` + `/dial`)
  - external media channel (`/channels/externalMedia`)
  - mixing bridge (`/bridges`, `/addChannel`)
- Connects to `/media/<MEDIA_WEBSOCKET_CONNECTION_ID>` and relays binary audio frames bidirectionally.
- Performs deterministic teardown (hang up channels + destroy bridge).

## Environment
- `ARI_BASE_URL` (default `http://127.0.0.1:8088/ari`)
- `ARI_USERNAME` (default `slmodem`)
- `ARI_PASSWORD` (default `slmodem`)
- `ARI_APP` (default `slmodem`)
- `ARI_DIAL_ENDPOINT_TEMPLATE` (default `PJSIP/{dial}@telnyx-out`)
- `ARI_ORIGINATE_TIMEOUT_SECS` (default `60`)
- `ARI_EXTERNAL_HOST` (default `INCOMING`)
- `ARI_MEDIA_FORMAT` (default `ulaw`)
- `ARI_MEDIA_TRANSPORT` (default `websocket`)
- `ARI_MEDIA_ENCAPSULATION` (default `none`)
- `ARI_MEDIA_CONNECTION_TYPE` (default `server`)
- `ARI_MEDIA_TRANSPORT_DATA` (recommended `f(json)`)
- `BRIDGE_CONNECT_TIMEOUT_MS` (default `3000`)

## Build and test
```bash
cargo build --release
cargo test
```

## License
Apache-2.0 (see `LICENSE`).
