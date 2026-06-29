ARG AIRBYTE_TO_FLOW_TAG="dev"
FROM ghcr.io/estuary/airbyte-to-flow:$AIRBYTE_TO_FLOW_TAG as airbyte-to-flow

FROM airbyte/source-amazon-ads:7.3.1

COPY --from=airbyte-to-flow /airbyte-to-flow ./
ENTRYPOINT ["/airbyte/integration_code/airbyte-to-flow", "--connector-entrypoint", "python /airbyte/integration_code/main.py"]

COPY *.json ./
COPY streams/* ./streams/

# source-amazon-ads 7.x is a modern-CDK connector that reads `--state` as a list
# of per-stream AirbyteStateMessages. This tells airbyte-to-flow to convert the
# persisted `{stream_name: stream_state}` state into that form.
ENV AIRBYTE_TO_FLOW_STATE_FORMAT=per_stream

LABEL io.airbyte.version=v2
LABEL io.airbyte.name=airbyte/source-amazon-ads
LABEL FLOW_RUNTIME_PROTOCOL=capture
LABEL CONNECTOR_PROTOCOL=flow-capture
LABEL FLOW_RUNTIME_CODEC=json
