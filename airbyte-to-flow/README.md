# Build

```
make && docker buildx build --platform linux/amd64 -t ghcr.io/estuary/airbyte-to-flow:local --push .
```

# Building on M1

Before running make, set this environment variable:
```
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker=musl-gcc"
```

# Configuration

## State format

Airbyte connectors read their incoming state from a `--state` file and emit
updated state as `STATE` messages. The Airbyte protocol defines three state types
([protocol reference](https://docs.airbyte.com/platform/understanding-airbyte/airbyte-protocol#state-types)):

- **LEGACY** — a single opaque JSON object (a "black box"), conventionally keyed by
  stream name: `{"<stream>": <stream_state>, ...}`. Emitted by older-CDK connectors.
- **STREAM** — per-stream state, written as a JSON _list_ of `AirbyteStateMessage`s,
  one per stream: `[{"type": "STREAM", "stream": {"stream_descriptor": {"name":
  "<stream>"}, "stream_state": <stream_state>}}, ...]`. The recommended format for
  modern-CDK connectors.
- **GLOBAL** — shared state plus per-stream state, used by CDC-style connectors:
  `{"type": "GLOBAL", "global": {"shared_state": {...}, "stream_states": [...]}}`.

Airbyte has [deprecated the LEGACY format](https://github.com/airbytehq/airbyte/discussions/39430)
(Airbyte Platform v0.62.4 / Python CDK ≥ 1.3, mid-2024) in favor of STREAM and
GLOBAL. The practical consequence: when a connector is upgraded across that
boundary it stops reading the LEGACY object and starts requiring a STREAM list —
and its `read_state` rejects the object form with `"<stream name>" is not of type
"object"`.

Flow always persists Airbyte state as a `{"<stream>": <stream_state>}` object,
regardless of connector. airbyte-to-flow translates between that object and
whatever the connector actually speaks, in each direction:

**Inbound (`--state` file)** is controlled by the `AIRBYTE_TO_FLOW_STATE_FORMAT`
environment variable, set in the connector's Dockerfile:

```dockerfile
# Needed for modern-CDK connectors that read `--state` as a per-stream STREAM list.
ENV AIRBYTE_TO_FLOW_STATE_FORMAT=per_stream
```

- `per_stream` — convert the persisted `{"<stream>": <stream_state>}` object into a
  STREAM list before invoking the connector.
- unset (the default) — write the persisted state verbatim. Use this for
  LEGACY-state connectors; setting `per_stream` on one would reshape its opaque
  state and can cause state loss.

**Outbound (`STATE` messages)** is detected automatically from what the connector
emits, so it needs no configuration:

- a LEGACY `data` blob is forwarded as-is (preserving its merge flag);
- a per-stream `STREAM` message is persisted as `{"<stream>": <stream_state>}` via
  RFC 7396 JSON merge patch, so each stream updates only its own key and states
  accumulate into the object above;
- anything else (e.g. GLOBAL) is not yet supported — it is logged and skipped, so
  the capture continues but that state is not persisted.
