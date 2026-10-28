ARG AIRBYTE_TO_FLOW_TAG="dev"
FROM ghcr.io/estuary/airbyte-to-flow:$AIRBYTE_TO_FLOW_TAG as airbyte-to-flow

FROM airbyte/source-google-search-console:0.1.22

# A network error (DNS, connection reset) during the OAuth token refresh has no HTTP response,
# but this CDK reads e.response.status_code anyway and hides the real error behind a NoneType
# AttributeError. Later CDK releases add the same None guard.
RUN sed -i 's/if e.response.status_code == 429 or e.response.status_code >= 500:/if e.response is not None and (e.response.status_code == 429 or e.response.status_code >= 500):/' \
        /usr/local/lib/python3.9/site-packages/airbyte_cdk/sources/streams/http/auth/oauth.py \
    && grep -q "if e.response is not None and" /usr/local/lib/python3.9/site-packages/airbyte_cdk/sources/streams/http/auth/oauth.py

COPY --from=airbyte-to-flow /airbyte-to-flow ./
ENTRYPOINT ["/airbyte/integration_code/airbyte-to-flow", "--connector-entrypoint", "python /airbyte/integration_code/main.py"]

COPY documentation_url.patch.json ./
COPY spec.patch.json ./
COPY spec.map.json ./
COPY oauth2.patch.json ./
COPY streams/* ./streams/

LABEL io.airbyte.version=v2
LABEL io.airbyte.name=airbyte/source-google-search-console
LABEL FLOW_RUNTIME_PROTOCOL=capture
LABEL CONNECTOR_PROTOCOL=flow-capture
LABEL FLOW_RUNTIME_CODEC=json