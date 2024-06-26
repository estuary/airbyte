use crate::libs::airbyte_catalog::Message;
use crate::{apis::InterceptorStream, errors::create_custom_error};

use crate::errors::{io_stream_to_interceptor_stream, Error};
use bytelines::AsyncByteLines;
use bytes::Bytes;
use futures::{StreamExt, TryStream, TryStreamExt};
use tokio_util::io::StreamReader;
use validator::Validate;

use super::airbyte_catalog::{Log, LogLevel, MessageType};

// Creates a stream of bytes of lines from the given stream
// This allows our other methods such as stream_airbyte_responses to operate
// on lines, simplifying their logic
pub fn stream_lines(
    in_stream: InterceptorStream,
) -> impl TryStream<Item = Result<Bytes, Error>, Error = Error, Ok = bytes::Bytes> {
    io_stream_to_interceptor_stream(
        AsyncByteLines::new(StreamReader::new(in_stream))
            .into_stream()
            .map_ok(Bytes::from),
    )
}

/// Given a stream of lines, try to deserialize them into Airbyte Messages.
/// This can be used when reading responses from the Airbyte connector, and will
/// handle validation of messages as well as handling of AirbyteLogMessages.
/// Will ignore* lines that cannot be parsed to an AirbyteMessage.
/// * See https://docs.airbyte.com/understanding-airbyte/airbyte-specification#the-airbyte-protocol
pub fn stream_airbyte_responses(
    in_stream: InterceptorStream,
) -> impl TryStream<Item = Result<Message, Error>, Ok = Message, Error = Error> {
    stream_lines(in_stream).try_filter_map(|line| async move {
        let message: Message = match serde_json::from_slice(&line) {
            Ok(m) => m,
            Err(e) => {
                // It is currently ambiguous for us whether Airbyte protocol specification
                // mandates that there must be no plaintext or not, as such we handle all
                // errors in parsing of stdout lines by logging the issue, but not failing
                Message {
                    message_type: MessageType::Log,
                    connection_status: None,
                    state: None,
                    record: None,
                    spec: None,
                    catalog: None,
                    log: Some(Log {
                        level: LogLevel::Debug,
                        message: format!("Encountered error while trying to parse ATF Message: {:?} in line {:?}", e, line)
                    }),
                    trace: None,
                }
            }
        };

        message
            .validate()
            .map_err(|e| create_custom_error(&format!("error in validating message {:?}", e)))?;

        Ok(Some(message))
    })
    .try_filter_map(|message| async {
        // For logs and traces, log them and then filter them out
        // so that we don't have to handle them elsewhere
        if let Some(log) = message.log {
            log.log();
            Ok(None)
        } else if let Some(trace) = message.trace {
            trace.trace();
            Ok(None)
        } else {
            Ok(Some(message))
        }
    })
}

/// Read the given stream and try to find an Airbyte message that matches the predicate
/// ignoring* other message kinds. This can be used to work with Airbyte connector responses.
/// * See https://docs.airbyte.com/understanding-airbyte/airbyte-specification#the-airbyte-protocol
pub fn get_airbyte_response<F: 'static>(
    in_stream: InterceptorStream,
    predicate: F,
    descriptor: &'static str,
) -> impl futures::Future<Output = Result<Message, Error>>
where
    F: Fn(&Message) -> bool,
{
    async move {
        let stream_head = Box::pin(stream_airbyte_responses(in_stream)).next().await;

        let message = match stream_head {
            Some(m) => m,
            None => return Err(Error::EmptyStream),
        }?;

        if predicate(&message) {
            Ok(message)
        } else {
            Err(Error::MessageNotFound(descriptor))
        }
    }
}

/// Read the given stream of bytes from runtime and try to decode it to type <T>
pub fn get_decoded_message<T>(
    in_stream: InterceptorStream,
) -> impl futures::Future<Output = Result<T, Error>>
where
    T: prost::Message + std::default::Default + for<'a> serde::Deserialize<'a>,
{
    async move {
        let msg = stream_lines(in_stream)
            .next()
            .await
            .ok_or(Error::MessageNotFound(std::any::type_name::<T>()))??;
        let v = serde_json::from_slice(&msg)?;
        Ok(v)
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeMap, pin::Pin};

    use bytes::BytesMut;
    use futures::stream;
    use proto_flow::{flow::materialization_spec::ConnectorType, materialize::request};
    use tokio_util::io::ReaderStream;

    use crate::libs::airbyte_catalog::{ConnectionStatus, MessageType, Status};

    use super::*;

    fn create_stream<T>(
        input: Vec<T>,
    ) -> Pin<Box<impl TryStream<Item = Result<T, Error>, Ok = T, Error = Error>>> {
        Box::pin(stream::iter(input.into_iter().map(Ok::<T, Error>)))
    }

    #[tokio::test]
    async fn test_stream_lines() {
        let line_0 = "{\"test\": \"hello\"}".as_bytes();
        let line_1 = "other".as_bytes();
        let line_2 = "{\"object\": {}}".as_bytes();
        let newline = "\n".as_bytes();
        let mut input = BytesMut::new();
        input.extend_from_slice(line_0);
        input.extend_from_slice(newline);
        input.extend_from_slice(line_1);
        input.extend_from_slice(newline);
        input.extend_from_slice(line_2);
        let stream = create_stream(vec![Bytes::from(input)]);
        let all_bytes = Box::pin(stream_lines(stream));

        let result: Vec<Bytes> = all_bytes.try_collect::<Vec<Bytes>>().await.unwrap();
        assert_eq!(result, vec![line_0, line_1, line_2]);
    }

    #[tokio::test]
    async fn test_stream_airbyte_responses_eof_split_json() {
        let input_message = Message {
            message_type: MessageType::ConnectionStatus,
            log: None,
            state: None,
            record: None,
            spec: None,
            catalog: None,
            trace: None,
            connection_status: Some(ConnectionStatus {
                status: Status::Succeeded,
                message: Some("test".to_string()),
            }),
        };
        let input = vec![
            Bytes::from("{\"type\": \"CONNECTION_STATUS\", \"connectionStatus\": {"),
            Bytes::from("\"status\": \"SUCCEEDED\",\"message\":\"test\"}}"),
        ];
        let stream = create_stream(input);

        let mut messages = Box::pin(stream_airbyte_responses(stream));

        let result = messages.next().await.unwrap().unwrap();
        assert_eq!(
            result.connection_status.unwrap(),
            input_message.connection_status.unwrap()
        );
    }

    #[tokio::test]
    async fn test_stream_airbyte_responses_eof_split_json_partial() {
        let input_message = Message {
            message_type: MessageType::ConnectionStatus,
            log: None,
            state: None,
            record: None,
            spec: None,
            catalog: None,
            trace: None,
            connection_status: Some(ConnectionStatus {
                status: Status::Succeeded,
                message: Some("test".to_string()),
            }),
        };
        let input = vec![
            Bytes::from("{}\n{\"type\": \"CONNECTION_STATUS\", \"connectionStatus\": {"),
            Bytes::from("\"status\": \"SUCCEEDED\",\"message\":\"test\"}}"),
        ];
        let stream = create_stream(input);

        let mut messages = Box::pin(stream_airbyte_responses(stream));

        let result = messages.next().await.unwrap().unwrap();
        assert_eq!(
            result.connection_status.unwrap(),
            input_message.connection_status.unwrap()
        );
    }

    #[tokio::test]
    async fn test_stream_airbyte_responses_plaintext_mixed() {
        let input_message = Message {
            message_type: MessageType::ConnectionStatus,
            log: None,
            state: None,
            record: None,
            spec: None,
            catalog: None,
            trace: None,
            connection_status: Some(ConnectionStatus {
                status: Status::Succeeded,
                message: Some("test".to_string()),
            }),
        };
        let input = vec![
            Bytes::from(
                "I am plaintext!\n{\"type\": \"CONNECTION_STATUS\", \"connectionStatus\": {",
            ),
            Bytes::from("\"status\": \"SUCCEEDED\",\"message\":\"test\"}}"),
        ];
        let stream = create_stream(input);

        let mut messages = Box::pin(stream_airbyte_responses(stream));

        let result = messages.next().await.unwrap().unwrap();
        assert_eq!(
            result.connection_status.unwrap(),
            input_message.connection_status.unwrap()
        );
    }

    #[tokio::test]
    async fn test_get_decoded_message() {
        let msg = request::Validate {
            name: "materialization".to_string(),
            connector_type: ConnectorType::Image.into(),
            config_json: "{}".to_string(),
            bindings: vec![request::validate::Binding {
                resource_config_json: "{}".to_string(),
                collection: None,
                field_config_json_map: BTreeMap::new(),
                backfill: 7,
            }],
            last_materialization: None,
            last_version: String::new(),
        };

        let msg_buf = serde_json::to_string(&msg).unwrap();
        let read_stream =
            io_stream_to_interceptor_stream(ReaderStream::new(std::io::Cursor::new(msg_buf)));

        let stream: InterceptorStream = Box::pin(read_stream);
        let result = get_decoded_message::<request::Validate>(stream)
            .await
            .unwrap();

        assert_eq!(result, msg);
    }
}
