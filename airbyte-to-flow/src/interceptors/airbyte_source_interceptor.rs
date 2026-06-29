use crate::apis::InterceptorStream;
use crate::libs::json::{create_root_schema, tokenize_jsonpointer};

use crate::errors::Error;
use crate::libs::airbyte_catalog::{
    self, ConfiguredCatalog, ConfiguredStream, DestinationSyncMode, Range, ResourceSpec, Status,
    SyncMode,
};
use crate::libs::command::READY;
use crate::libs::stream::{get_airbyte_response, stream_airbyte_responses};

use bytes::Bytes;
use proto_flow::capture::{request, response, Request, Response};
use proto_flow::flow::ConnectorState;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use validator::Validate;

use futures::{stream, StreamExt};
use serde_json as sj;
use std::fs::File;
use std::io::Write;
use tempfile::{Builder, TempDir};

use json_patch::merge;

use super::fix_document_schema::{
    fix_document_schema_keys, fix_nonstandard_jsonschema_attributes,
    normalize_schema_date_to_datetime, remove_enums,
};
use super::normalize::{automatic_normalizations, normalize_doc, NormalizationEntry};
use super::remap::remap;

const PROTOCOL_VERSION: u32 = 3032023;

const CONFIG_FILE_NAME: &str = "config.json";
const CATALOG_FILE_NAME: &str = "catalog.json";
const STATE_FILE_NAME: &str = "state.json";

const SPEC_PATCH_FILE_NAME: &str = "spec.patch.json";
const SPEC_MAP_FILE_NAME: &str = "spec.map.json";
const OAUTH2_PATCH_FILE_NAME: &str = "oauth2.patch.json";
const DOC_URL_PATCH_FILE_NAME: &str = "documentation_url.patch.json";
const SCHEMA_NORMALIZATIONS_FILE_NAME: &str = "schema_normalizations.json";
const STREAM_PATCH_DIR_NAME: &str = "streams";
const STREAM_PK_SUFFIX: &str = ".pk.json";
const STREAM_PATCH_SUFFIX: &str = ".patch.json";
const STREAM_NORMALIZE_SUFFIX: &str = ".normalize.json";
const SELECTED_STREAMS_FILE_NAME: &str = "selected_streams.json";
const OMITTED_STREAMS_FILE_NAME: &str = "omitted_streams.json";

// Optional per-connector env var declaring the `--state` format the
// connector expects, set in the connector's Dockerfile. When it equals
// "per_stream", the persisted `{stream_name: stream_state}` state is converted
// into a list of per-stream AirbyteStateMessages before being handed to the
// connector. Unset (the default), state is written verbatim, preserving behavior
// for legacy/global-state connectors.
const STATE_FORMAT_ENV_VAR: &str = "AIRBYTE_TO_FLOW_STATE_FORMAT";
const STATE_FORMAT_PER_STREAM: &str = "per_stream";

// SavedBinding records the binding index and applicable normalizations obtained from a Pull
// request.
struct SavedBinding {
    i: usize,
    normalizations: Option<Vec<NormalizationEntry>>,
    doc_schema: serde_json::Value,
}

pub struct AirbyteSourceInterceptor {
    validate_request: Arc<Mutex<Option<request::Validate>>>,
    stream_to_binding: Arc<Mutex<HashMap<String, SavedBinding>>>,
    tmp_dir: TempDir,
}

impl AirbyteSourceInterceptor {
    pub fn new() -> Self {
        AirbyteSourceInterceptor {
            validate_request: Arc::new(Mutex::new(None)),
            stream_to_binding: Arc::new(Mutex::new(HashMap::new())),
            tmp_dir: Builder::new()
                .prefix("atf-")
                .tempdir_in("/var/tmp")
                .expect("failed to create temp dir."),
        }
    }

    fn adapt_spec_request_stream(&mut self, _request: request::Spec) -> InterceptorStream {
        Box::pin(stream::once(async move { Ok(Bytes::from(READY)) }))
    }

    fn adapt_spec_response_stream(&mut self, in_stream: InterceptorStream) -> InterceptorStream {
        Box::pin(stream::once(async {
            let message = get_airbyte_response(in_stream, |m| m.spec.is_some(), "spec").await?;
            let spec = message.spec.unwrap();
            let mut endpoint_spec = sj::from_str::<sj::Value>(spec.connection_specification.get())?;
            let mut auth_spec = spec
                .auth_specification
                .map(|aspec| sj::from_str::<sj::Value>(aspec.get()))
                .transpose()?;

            let spec_patch = std::fs::read_to_string(SPEC_PATCH_FILE_NAME)
                .ok()
                .map(|p| sj::from_str::<sj::Value>(&p))
                .transpose()?;
            let oauth2_patch = std::fs::read_to_string(OAUTH2_PATCH_FILE_NAME)
                .ok()
                .map(|p| sj::from_str::<sj::Value>(&p))
                .transpose()?;
            let documentation_url_patch = std::fs::read_to_string(DOC_URL_PATCH_FILE_NAME)
                .ok()
                .map(|p| sj::from_str::<sj::Value>(&p))
                .transpose()?;

            if let Some(p) = spec_patch {
                merge(&mut endpoint_spec, &p);
            }

            if let Some(p) = oauth2_patch.as_ref() {
                auth_spec = Some(p.clone());
            }

            let documentation_url = match documentation_url_patch {
                Some(p) => p
                    .get("documentation_url")
                    .map(|v| v.as_str())
                    .flatten()
                    .map(|s| s.to_string()),
                None => spec.documentation_url,
            };

            fix_nonstandard_jsonschema_attributes(&mut endpoint_spec);

            let v = serde_json::to_vec(&Response {
                spec: Some(response::Spec {
                    protocol: PROTOCOL_VERSION,
                    config_schema_json: endpoint_spec.to_string().into(),
                    resource_config_schema_json: serde_json::to_string(&create_root_schema::<
                        ResourceSpec,
                    >())?.into(),
                    oauth2: auth_spec
                        .map(|spec| serde_json::from_value(spec))
                        .transpose()?,
                    documentation_url: documentation_url.unwrap_or_default(),
                    resource_path_pointers: vec!["/namespace".to_string(), "/stream".to_string()],
                }),
                ..Default::default()
            })?;

            Ok(v.into())
        }))
    }

    fn adapt_config_json(config_json: &[u8]) -> Result<sj::Value, Error> {
        let spec_map = std::fs::read_to_string(SPEC_MAP_FILE_NAME)
            .ok()
            .map(|p| sj::from_str::<sj::Value>(&p))
            .transpose()?;
        let mut spec = sj::from_slice::<sj::Value>(config_json)?;
        if let Some(mapping) = spec_map.as_ref() {
            remap(&mut spec, &mapping)?;
        }

        Ok(spec)
    }

    fn adapt_discover_request(
        &mut self,
        config_file_path: String,
        request: request::Discover,
    ) -> InterceptorStream {
        Box::pin(stream::once(async move {
            let config_json = AirbyteSourceInterceptor::adapt_config_json(&request.config_json)?;

            File::create(config_file_path)?.write_all(config_json.to_string().as_bytes())?;

            Ok(Bytes::from(READY))
        }))
    }

    fn adapt_discover_response_stream(
        &mut self,
        in_stream: InterceptorStream,
    ) -> InterceptorStream {
        Box::pin(stream::once(async {
            let message =
                get_airbyte_response(in_stream, |m| m.catalog.is_some(), "catalog").await?;
            let catalog = message.catalog.unwrap();

            let selected_streams_option = std::fs::read_to_string(SELECTED_STREAMS_FILE_NAME)
                .ok()
                .map(|p| sj::from_str::<Vec<String>>(&p))
                .transpose()?;

            let omitted_streams_option = std::fs::read_to_string(OMITTED_STREAMS_FILE_NAME)
                .ok()
                .map(|p| sj::from_str::<Vec<String>>(&p))
                .transpose()?;

            let schema_normalizations = std::fs::read_to_string(SCHEMA_NORMALIZATIONS_FILE_NAME)
                .ok()
                .map(|p| sj::from_str::<Vec<String>>(&p))
                .transpose()?
                .unwrap_or(Vec::new());

            let mut resp = response::Discovered::default();
            for stream in catalog.streams {
                if let Some(ref omitted_streams) = omitted_streams_option {
                    if omitted_streams.contains(&stream.name) {
                        continue;
                    }
                }

                let mut disable = false;
                if let Some(ref selected_streams) = selected_streams_option {
                    if !selected_streams.contains(&stream.name) {
                        disable = true;
                    }
                }
                let recommended_name = stream_to_recommended_name(&stream.name);

                let has_incremental = stream
                    .supported_sync_modes
                    .map(|modes| modes.contains(&SyncMode::Incremental))
                    .unwrap_or(false);
                let mode = if has_incremental {
                    SyncMode::Incremental
                } else {
                    SyncMode::FullRefresh
                };

                let mut key: Vec<String> = stream
                    .source_defined_primary_key
                    .unwrap_or(Vec::new())
                    .iter()
                    .map(|key| {
                        json::Pointer::from_iter(key.iter().map(|s| json::ptr::Token::from_str(&s)))
                            .to_string()
                    })
                    .collect();

                let doc_pk = std::fs::read_to_string(format!(
                    "{}/{}{}",
                    STREAM_PATCH_DIR_NAME, recommended_name, STREAM_PK_SUFFIX
                ))
                .or_else(|_| {
                    std::fs::read_to_string(format!(
                        "{}/*{}",
                        STREAM_PATCH_DIR_NAME, STREAM_PK_SUFFIX
                    ))
                })
                .ok()
                .map(|p| sj::from_str::<sj::Value>(&p))
                .transpose()?;
                if let Some(p) = doc_pk {
                    key = p
                        .as_array()
                        .ok_or(Error::InvalidPKPatch("expected an array".to_string()))?
                        .into_iter()
                        .map(|s| format!("/{}", s.as_str().unwrap()))
                        .collect();
                }

                // cursor_field does not accept JSON Pointers, but keys directly, so we remove the initial `/` from keys
                let non_pointer_key = key
                    .iter()
                    .map(|ptr| ptr.get(1..).unwrap().to_string())
                    .collect();

                // Sometimes the cursor_field is Some([]), this block handles that case and defaults to the primary key
                let cursor_field = if let Some(cf) = stream.default_cursor_field {
                    if cf.is_empty() {
                        Some(non_pointer_key)
                    } else {
                        Some(cf)
                    }
                } else {
                    Some(non_pointer_key)
                };

                let resource_spec = ResourceSpec {
                    stream: stream.name.clone(),
                    namespace: stream.namespace,
                    sync_mode: mode,
                    cursor_field,
                };

                let mut doc_schema = sj::from_str::<sj::Value>(stream.json_schema.get())?;

                let doc_schema_patch = std::fs::read_to_string(format!(
                    "{}/{}{}",
                    STREAM_PATCH_DIR_NAME, recommended_name, STREAM_PATCH_SUFFIX
                ))
                .or_else(|_| {
                    std::fs::read_to_string(format!(
                        "{}/*{}",
                        STREAM_PATCH_DIR_NAME, STREAM_PATCH_SUFFIX
                    ))
                })
                .ok()
                .map(|p| sj::from_str::<sj::Value>(&p))
                .transpose()?;

                if let Some(p) = doc_schema_patch {
                    merge(&mut doc_schema, &p);
                }

                fix_nonstandard_jsonschema_attributes(&mut doc_schema);
                remove_enums(&mut doc_schema);

                for normalization in &schema_normalizations {
                    match normalization.as_str() {
                        "date-to-datetime" => {
                            normalize_schema_date_to_datetime(&mut doc_schema);
                        }
                        _ => {}
                    }
                }

                resp.bindings.push(response::discovered::Binding {
                    recommended_name,
                    resource_config_json: serde_json::to_string(&resource_spec)?.into(),
                    key: key.clone(),
                    document_schema_json: fix_document_schema_keys(doc_schema, key)?.to_string().into(),
                    disable,
                    resource_path: Vec::new(), // this is deprecated and unused
                    is_fallback_key: false,
                })
            }

            let v = serde_json::to_vec(&Response {
                discovered: Some(resp),
                ..Default::default()
            })?;
            Ok(v.into())
        }))
    }

    fn adapt_validate_request_stream(
        &mut self,
        config_file_path: String,
        validate_request: Arc<Mutex<Option<request::Validate>>>,
        request: request::Validate,
    ) -> InterceptorStream {
        Box::pin(stream::once(async move {
            *validate_request.lock().await = Some(request.clone());

            let config_json = AirbyteSourceInterceptor::adapt_config_json(&request.config_json)?;

            File::create(config_file_path)?.write_all(config_json.to_string().as_bytes())?;

            Ok(Bytes::from(READY))
        }))
    }

    fn adapt_validate_response_stream(
        &mut self,
        validate_request: Arc<Mutex<Option<request::Validate>>>,
        in_stream: InterceptorStream,
    ) -> InterceptorStream {
        Box::pin(stream::once(async move {
            let message = get_airbyte_response(
                in_stream,
                |m| m.connection_status.is_some(),
                "connection status",
            )
            .await?;

            let connection_status = message.connection_status.unwrap();

            if connection_status.status != Status::Succeeded {
                let msg = connection_status.message.unwrap_or("".to_string());
                return Err(Error::ConnectionStatusUnsuccessful(msg));
            }

            let req = validate_request.lock().await;
            let req = req.as_ref().ok_or(Error::MissingValidateRequest)?;
            let mut resp = response::Validated::default();
            for binding in &req.bindings {
                let resource: ResourceSpec = serde_json::from_slice(&binding.resource_config_json)?;
                resp.bindings.push(response::validated::Binding {
                    resource_path: resource_spec_to_resource_path(&resource),
                });
            }

            let v = serde_json::to_vec(&Response {
                validated: Some(resp),
                ..Default::default()
            })?;
            Ok(v.into())
        }))
    }

    fn adapt_apply_request_stream(&mut self, _request: request::Apply) -> InterceptorStream {
        Box::pin(stream::once(async move { Ok(Bytes::from(READY)) }))
    }

    fn adapt_apply_response_stream(&mut self, in_stream: InterceptorStream) -> InterceptorStream {
        Box::pin(stream::once(async {
            // TODO(johnny): Due to the current factoring, we invoke the connector with `spec`
            // and discard its response. This is a bit silly.
            _ = get_airbyte_response(in_stream, |m| m.spec.is_some(), "spec").await?;

            let v = serde_json::to_vec(&Response {
                applied: Some(response::Applied::default()),
                ..Default::default()
            })?;
            Ok(v.into())
        }))
    }

    fn adapt_pull_request_stream(
        &mut self,
        config_file_path: String,
        catalog_file_path: String,
        state_file_path: String,
        stream_to_binding: Arc<Mutex<HashMap<String, SavedBinding>>>,
        open: request::Open,
    ) -> InterceptorStream {
        Box::pin(stream::once(async move {
            // Connectors that expect per-stream list state opt in via STATE_FORMAT_ENV_VAR.
            let wants_per_stream = std::env::var(STATE_FORMAT_ENV_VAR)
                .map(|v| v == STATE_FORMAT_PER_STREAM)
                .unwrap_or(false);

            let mut state_file = File::create(state_file_path.clone())?;
            if wants_per_stream {
                state_file.write_all(&convert_state_for_connector(&open.state_json)?)?;
            } else {
                state_file.write_all(&open.state_json)?;
            }
            let c = open.capture.unwrap();

            let config_json = AirbyteSourceInterceptor::adapt_config_json(&c.config_json)?;

            File::create(config_file_path.clone())?
                .write_all(config_json.to_string().as_bytes())?;

            let mut catalog = ConfiguredCatalog {
                streams: Vec::new(),
                range: open.range.as_ref().map(|r| Range {
                    begin: r.key_begin,
                    end: r.key_end,
                }),
            };

            for (i, binding) in c.bindings.iter().enumerate() {
                let resource: ResourceSpec = serde_json::from_slice(&binding.resource_config_json)?;

                let normalizations = std::fs::read_to_string(format!(
                    "{}/{}{}",
                    STREAM_PATCH_DIR_NAME, &resource.stream, STREAM_NORMALIZE_SUFFIX
                ))
                .ok()
                .map(|p| sj::from_str::<Vec<NormalizationEntry>>(&p))
                .transpose()?;

                let mut projections = HashMap::new();
                if let Some(ref collection) = binding.collection {
                    stream_to_binding.lock().await.insert(
                        resource.stream.clone(),
                        SavedBinding {
                            i,
                            normalizations,
                            doc_schema: serde_json::from_slice(&collection.write_schema_json)?
                        },
                    );
                    for p in &collection.projections {
                        projections.insert(p.field.clone(), p.ptr.clone());
                    }

                    let primary_key: Vec<Vec<String>> = collection
                        .key
                        .iter()
                        .map(|ptr| tokenize_jsonpointer(ptr))
                        .collect();
                    catalog.streams.push(ConfiguredStream {
                        sync_mode: resource.sync_mode.clone(),
                        destination_sync_mode: DestinationSyncMode::Append,
                        cursor_field: resource.cursor_field,
                        primary_key: Some(primary_key),
                        stream: airbyte_catalog::Stream {
                            name: resource.stream,
                            namespace: resource.namespace,
                            json_schema: serde_json::from_slice(&collection.write_schema_json)?,
                            supported_sync_modes: Some(vec![resource.sync_mode.clone()]),
                            default_cursor_field: None,
                            source_defined_cursor: None,
                            source_defined_primary_key: None,
                        },
                        projections,
                    });
                }
            }

            if let Err(e) = catalog.validate() {
                return Err(Error::InvalidCatalog(e));
            }

            serde_json::to_writer(File::create(catalog_file_path.clone())?, &catalog)?;

            Ok(Bytes::from(READY))
        }))
    }

    fn adapt_pull_response_stream(
        &mut self,
        stream_to_binding: Arc<Mutex<HashMap<String, SavedBinding>>>,
        in_stream: InterceptorStream,
    ) -> InterceptorStream {
        // Respond first with Opened.
        let opened = stream::once(async {
            let v = serde_json::to_vec(&Response {
                opened: Some(Default::default()),
                ..Default::default()
            })?;
            Ok(v.into())
        });

        // Then stream airbyte messages converted to the native protocol.
        let airbyte_message_stream = Box::pin(stream_airbyte_responses(in_stream));
        let airbyte_message_stream = stream::try_unfold(
            (stream_to_binding, airbyte_message_stream),
            |(stb, mut stream)| async move {
                let message = match stream.next().await {
                    Some(m) => m?,
                    None => {
                        return Ok(None);
                    }
                };

                let mut resp = Response::default();
                if let Some(state) = message.state {
                    let (updated_json, merge_patch) = state_to_checkpoint(state)?;
                    resp.checkpoint = Some(response::Checkpoint {
                        state: Some(ConnectorState {
                            updated_json: updated_json.into(),
                            merge_patch,
                        }),
                    });

                    let v = serde_json::to_vec(&resp)?;
                    Ok(Some((v.into(), (stb, stream))))
                } else if let Some(mut record) = message.record {
                    let mut stream_to_binding = stb.lock().await;
                    let binding = stream_to_binding
                        .get_mut(&record.stream)
                        .ok_or(Error::DanglingConnectorRecord(record.stream.clone()))?;

                    automatic_normalizations(&mut record.data, &mut binding.doc_schema);
                    normalize_doc(&mut record.data, &binding.normalizations);

                    resp.captured = Some(response::Captured {
                        binding: binding.i as u32,
                        doc_json: record.data.to_string().into(),
                    });
                    drop(stream_to_binding);

                    let v = serde_json::to_vec(&resp)?;
                    Ok(Some((v.into(), (stb, stream))))
                } else {
                    Err(Error::InvalidPullResponse)
                }
            },
        );

        Box::pin(opened.chain(airbyte_message_stream))
    }

    fn input_file_path(&mut self, file_name: &str) -> String {
        self.tmp_dir
            .path()
            .join(file_name)
            .to_str()
            .expect("failed construct config file name.")
            .into()
    }
}

#[derive(Clone, PartialEq, Debug, Copy)]
pub enum Operation {
    Spec,
    Discover,
    Validate,
    Apply,
    Capture,
}

impl AirbyteSourceInterceptor {
    // Reads the next request from a persistent line stream and determines the operation
    pub async fn next_request_from_lines<S>(
        &mut self,
        line_stream: &mut S,
    ) -> Result<(Operation, Request), Error>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, Error>> + Unpin,
    {
        use futures::StreamExt;
        
        let msg = line_stream
            .next()
            .await
            .ok_or(Error::EmptyStream)??;
        let req: Request = serde_json::from_slice(&msg)?;

        if req.spec.is_some() {
            Ok((Operation::Spec, req))
        } else if req.discover.is_some() {
            Ok((Operation::Discover, req))
        } else if req.validate.is_some() {
            Ok((Operation::Validate, req))
        } else if req.apply.is_some() {
            Ok((Operation::Apply, req))
        } else if req.open.is_some() {
            Ok((Operation::Capture, req))
        } else {
            Err(Error::UnknownOperation(format!("{:#?}", req)))
        }
    }

    pub fn adapt_command_args(&mut self, op: &Operation) -> Vec<String> {
        let config_file_path = self.input_file_path(CONFIG_FILE_NAME);
        let catalog_file_path = self.input_file_path(CATALOG_FILE_NAME);
        let state_file_path = self.input_file_path(STATE_FILE_NAME);

        let airbyte_args = match op {
            Operation::Spec => vec!["spec"],
            Operation::Discover => vec!["discover", "--config", &config_file_path],
            Operation::Validate => vec!["check", "--config", &config_file_path],
            // TODO(johnny): These are effective no-ops, but as-written must invoke the connector.
            // We should refactor this.
            Operation::Apply => vec!["spec"],
            Operation::Capture => {
                vec![
                    "read",
                    "--config",
                    &config_file_path,
                    "--catalog",
                    &catalog_file_path,
                    "--state",
                    &state_file_path,
                ]
            }
        };

        airbyte_args.into_iter().map(|s| s.to_string()).collect()
    }

    pub fn adapt_request_stream(
        &mut self,
        op: &Operation,
        first_request: Request,
    ) -> Result<InterceptorStream, Error> {
        let config_file_path = self.input_file_path(CONFIG_FILE_NAME);
        let catalog_file_path = self.input_file_path(CATALOG_FILE_NAME);
        let state_file_path = self.input_file_path(STATE_FILE_NAME);

        match op {
            Operation::Spec => Ok(self.adapt_spec_request_stream(first_request.spec.unwrap())),
            Operation::Discover => {
                Ok(self.adapt_discover_request(config_file_path, first_request.discover.unwrap()))
            }
            Operation::Validate => Ok(self.adapt_validate_request_stream(
                config_file_path,
                Arc::clone(&self.validate_request),
                first_request.validate.unwrap(),
            )),
            // TODO(johnny): These are effective no-ops, but as-written must invoke the connector.
            // We should refactor this.
            Operation::Apply => Ok(self.adapt_apply_request_stream(first_request.apply.unwrap())),
            Operation::Capture => Ok(self.adapt_pull_request_stream(
                config_file_path,
                catalog_file_path,
                state_file_path,
                Arc::clone(&self.stream_to_binding),
                first_request.open.unwrap(),
            )),
        }
    }

    pub fn adapt_response_stream(
        &mut self,
        op: &Operation,
        in_stream: InterceptorStream,
    ) -> Result<InterceptorStream, Error> {
        match op {
            Operation::Spec => Ok(self.adapt_spec_response_stream(in_stream)),
            Operation::Discover => Ok(self.adapt_discover_response_stream(in_stream)),
            Operation::Validate => {
                Ok(self
                    .adapt_validate_response_stream(Arc::clone(&self.validate_request), in_stream))
            }
            Operation::Apply => Ok(self.adapt_apply_response_stream(in_stream)),
            Operation::Capture => {
                Ok(self.adapt_pull_response_stream(Arc::clone(&self.stream_to_binding), in_stream))
            }
        }
    }
}

/// Returns a resource path for the given resource spec. This is really just the
/// `stream` property, which corresponds to the `name` of the airbyte `Stream`.
/// This must be consistent with `stream_to_resource_path`, so that the resource
/// paths returned by `Discover` and `Validate` are the same.
fn resource_spec_to_resource_path(res: &ResourceSpec) -> Vec<String> {
    let mut path = Vec::new();
    if let Some(ns) = &res.namespace {
        path.push(ns.clone());
    }
    path.push(res.stream.clone());
    path
}

// stream names have no constraints.
// Strip and sanitize them to be valid collection names.
fn stream_to_recommended_name(stream: &str) -> String {
    stream
        .split('/')
        .map(|chunk| {
            chunk
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
                .collect()
        })
        .filter(|c: &String| !c.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Translates a connector state message into the `(updated_json, merge_patch)`
/// pair persisted as a Flow ConnectorState checkpoint.
///
/// A LEGACY-type `data` blob is handled first and forwarded verbatim with its merge
/// flag. Every connector emitting `data` (which covers connectors on older CDK versions)
/// follows this path.
///
/// Connectors that emit STREAM-type per-stream state reach the
/// `stream` branch. Their state is persisted as a `{stream_name: stream_state}`
/// document reduced via RFC 7396 JSON merge patch, so each stream's checkpoint
/// updates only its own key and the streams' states accumulate into a single
/// object. That object is the shape `convert_state_for_connector` reads back,
/// so state round-trips.
///
/// Note: per-stream state is keyed by stream name only; a namespace, if present,
/// is dropped. The connectors this targets do not use namespaces, and the
/// `{stream_name: stream_state}` object form has no notion of one.
fn state_to_checkpoint(state: airbyte_catalog::State) -> Result<(String, bool), Error> {
    if let Some(data) = state.data {
        Ok((data.get().to_string(), state.merge.unwrap_or(false)))
    } else if let Some(stream_state) = state.stream {
        let inner: sj::Value = match stream_state.stream_state {
            Some(raw) => sj::from_str(raw.get())?,
            None => sj::Value::Object(sj::Map::new()),
        };
        let mut doc = sj::Map::new();
        doc.insert(stream_state.stream_descriptor.name, inner);
        Ok((sj::to_string(&sj::Value::Object(doc))?, true))
    } else {
        // Unrecognized state shape (e.g. GLOBAL/global state used by CDC connectors),
        // which we can't round-trip through the `{stream_name: stream_state}` object
        // form. Emit an empty merge patch so the capture continues without persisting
        // meaningless state, rather than erroring. Log the message type and any
        // `global` payload so we can tell what it was and whether to add support.
        tracing::warn!(
            state_type = state.state_type.as_deref().unwrap_or("<unknown>"),
            global = state.global.as_ref().map(|g| g.get()).unwrap_or("null"),
            "airbyte-to-flow received an unsupported Airbyte state message; state was not persisted for this checkpoint",
        );
        Ok(("{}".to_string(), true))
    }
}

/// Converts the persisted Flow connector state into the `--state` file
/// content expected by the connector.
///
/// Flow persists Airbyte state as a JSON object keyed by stream name
/// (`{stream_name: stream_state}`). This is both the shape produced by
/// `adapt_pull_response_stream` for STREAM-type connectors and the conventional
/// LEGACY format emitted by older-CDK connectors. Modern-CDK connectors
/// require the `--state` file to contain a JSON list of AirbyteStateMessages;
/// passing the object form makes their `read_state` iterate the object's keys and
/// fail with `"<stream name>" is not of type "object"`.
///
/// This converts the object form into the per-stream list the connector expects.
/// A value that is already a list is passed through unchanged (so connectors that
/// round-trip the list form directly keep working), and empty/absent/`null` state
/// becomes an empty list.
fn convert_state_for_connector(state_json: &[u8]) -> Result<Vec<u8>, Error> {
    let state_value: sj::Value = if state_json.is_empty() {
        sj::Value::Object(sj::Map::new())
    } else {
        sj::from_slice(state_json)?
    };

    let converted = match state_value {
        // Already a list of AirbyteStateMessages: pass through unchanged.
        sj::Value::Array(_) => state_value,
        // `{stream_name: stream_state}` -> [{type: STREAM, stream: {...}}, ...]
        sj::Value::Object(map) => sj::Value::Array(
            map.into_iter()
                .map(|(name, stream_state)| {
                    sj::json!({
                        "type": "STREAM",
                        "stream": {
                            "stream_descriptor": { "name": name },
                            "stream_state": stream_state,
                        }
                    })
                })
                .collect(),
        ),
        // Anything else (e.g. `null`): treat as no state.
        _ => sj::Value::Array(Vec::new()),
    };

    Ok(sj::to_vec(&converted)?)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_stream_to_recommended_name() {
        assert_eq!(stream_to_recommended_name("Hello-World"), "Hello-World");
        assert_eq!(
            stream_to_recommended_name("/&foo!/B ar// b+i-n.g /"),
            "foo/Bar/bi-n.g"
        );
    }

    #[test]
    fn test_resource_paths() {
        let spec_a = ResourceSpec {
            stream: "foo".to_string(),
            namespace: None,
            sync_mode: SyncMode::Incremental,
            cursor_field: None,
        };

        let expected = vec!["foo".to_string()];
        assert_eq!(&expected, &resource_spec_to_resource_path(&spec_a));

        let spec_b = ResourceSpec {
            namespace: Some("toe".to_string()),
            ..spec_a
        };
        let expected = vec!["toe".to_string(), "foo".to_string()];
        assert_eq!(&expected, &resource_spec_to_resource_path(&spec_b));
    }

    // Parses a `--state` file's bytes back into a value for assertions.
    fn parse(bytes: Vec<u8>) -> sj::Value {
        sj::from_slice(&bytes).unwrap()
    }

    #[test]
    fn test_convert_state_legacy_dict_to_per_stream_list() {
        // The legacy/canonical Flow state for an amazon-ads capture: a JSON object
        // keyed by stream name. The modern CDK requires this to be a list of
        // AirbyteStateMessages, otherwise read_state iterates the keys and fails
        // with `"<stream name>" is not of type "object"`.
        let legacy = br#"{
            "sponsored_brands_report_stream": {"12345": "2024-01-01"},
            "sponsored_products_campaigns": {"cursor": "2024-02-02"}
        }"#;

        let got = parse(convert_state_for_connector(legacy).unwrap());

        let expected = sj::json!([
            {
                "type": "STREAM",
                "stream": {
                    "stream_descriptor": {"name": "sponsored_brands_report_stream"},
                    "stream_state": {"12345": "2024-01-01"}
                }
            },
            {
                "type": "STREAM",
                "stream": {
                    "stream_descriptor": {"name": "sponsored_products_campaigns"},
                    "stream_state": {"cursor": "2024-02-02"}
                }
            }
        ]);
        assert_eq!(got, expected);
    }

    #[test]
    fn test_convert_state_empty_and_passthrough() {
        // Empty input -> empty list (a fresh capture must not crash read_state).
        assert_eq!(parse(convert_state_for_connector(b"").unwrap()), sj::json!([]));
        assert_eq!(parse(convert_state_for_connector(b"{}").unwrap()), sj::json!([]));
        // `null` -> empty list.
        assert_eq!(parse(convert_state_for_connector(b"null").unwrap()), sj::json!([]));
        // A list is already in the connector's expected form and is passed through.
        let list = br#"[{"type":"STREAM","stream":{"stream_descriptor":{"name":"s"},"stream_state":{"c":1}}}]"#;
        assert_eq!(
            parse(convert_state_for_connector(list).unwrap()),
            parse(list.to_vec())
        );
    }

    #[test]
    fn test_state_to_checkpoint_per_stream() {
        // A modern per-stream STATE message is persisted as `{name: stream_state}`
        // with merge_patch=true, so each stream updates only its own key.
        let state: airbyte_catalog::State = sj::from_str(
            r#"{"type":"STREAM","stream":{"stream_descriptor":{"name":"sponsored_brands_v3_report_stream"},"stream_state":{"reportDate":"2024-03-03"}}}"#,
        )
        .unwrap();

        let (updated_json, merge_patch) = state_to_checkpoint(state).unwrap();
        assert!(merge_patch);
        assert_eq!(
            sj::from_str::<sj::Value>(&updated_json).unwrap(),
            sj::json!({"sponsored_brands_v3_report_stream": {"reportDate": "2024-03-03"}})
        );
    }

    #[test]
    fn test_state_to_checkpoint_legacy_data() {
        // A legacy `data` blob is forwarded as-is, preserving its merge flag.
        let state: airbyte_catalog::State =
            sj::from_str(r#"{"data":{"some":"state"},"merge":true}"#).unwrap();
        let (updated_json, merge_patch) = state_to_checkpoint(state).unwrap();
        assert!(merge_patch);
        assert_eq!(
            sj::from_str::<sj::Value>(&updated_json).unwrap(),
            sj::json!({"some": "state"})
        );

        // Without a merge flag, legacy state defaults to a full replacement.
        let state: airbyte_catalog::State =
            sj::from_str(r#"{"data":{"some":"state"}}"#).unwrap();
        let (_, merge_patch) = state_to_checkpoint(state).unwrap();
        assert!(!merge_patch);
    }

    #[test]
    fn test_state_to_checkpoint_prefers_data_when_both_present() {
        // Some connectors emit both a legacy `data` blob and per-stream `stream`
        // for backwards compatibility. `data` must win so these connectors keep
        // their pre-existing behavior unchanged.
        let state: airbyte_catalog::State = sj::from_str(
            r#"{"type":"STREAM","data":{"legacy":"blob"},"stream":{"stream_descriptor":{"name":"s"},"stream_state":{"c":1}}}"#,
        )
        .unwrap();
        let (updated_json, merge_patch) = state_to_checkpoint(state).unwrap();
        assert!(!merge_patch);
        assert_eq!(
            sj::from_str::<sj::Value>(&updated_json).unwrap(),
            sj::json!({"legacy": "blob"})
        );
    }

    #[test]
    fn test_state_to_checkpoint_unrecognized_is_noop() {
        // Unrecognized state (e.g. GLOBAL) becomes an empty merge patch (a no-op)
        // rather than erroring the capture.
        let state: airbyte_catalog::State = sj::from_str(
            r#"{"type":"GLOBAL","global":{"shared_state":{"x":1},"stream_states":[]}}"#,
        )
        .unwrap();

        // The fields we don't consume are captured so the warning log can report
        // what the unsupported message actually was.
        assert_eq!(state.state_type.as_deref(), Some("GLOBAL"));
        assert_eq!(
            sj::from_str::<sj::Value>(state.global.as_ref().unwrap().get()).unwrap(),
            sj::json!({"shared_state":{"x":1},"stream_states":[]})
        );

        let (updated_json, merge_patch) = state_to_checkpoint(state).unwrap();
        assert!(merge_patch);
        assert_eq!(sj::from_str::<sj::Value>(&updated_json).unwrap(), sj::json!({}));
    }

    #[test]
    fn test_state_round_trips_through_both_directions() {
        // Full cycle: a modern per-stream checkpoint is persisted, then read back
        // out to the connector. The stream's cursor must survive intact.
        let state: airbyte_catalog::State = sj::from_str(
            r#"{"type":"STREAM","stream":{"stream_descriptor":{"name":"sponsored_products_campaigns"},"stream_state":{"cursor":"2024-04-04"}}}"#,
        )
        .unwrap();

        // Outbound: connector -> Flow ConnectorState.
        let (persisted, _) = state_to_checkpoint(state).unwrap();

        // Inbound: the persisted Flow state -> connector `--state` file.
        let for_connector = parse(convert_state_for_connector(persisted.as_bytes()).unwrap());

        assert_eq!(
            for_connector,
            sj::json!([
                {
                    "type": "STREAM",
                    "stream": {
                        "stream_descriptor": {"name": "sponsored_products_campaigns"},
                        "stream_state": {"cursor": "2024-04-04"}
                    }
                }
            ])
        );
    }
}
