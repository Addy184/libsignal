//
// Copyright 2024 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use libsignal_net_infra::errors::{LogSafeDisplay, TransportConnectError};
use libsignal_net_infra::ws::{WebSocketConnectError, WebSocketServiceError};
use libsignal_net_infra::{extract_retry_after_seconds, service};

use crate::ws::WebSocketServiceConnectError;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum ChatServiceError {
    /// websocket error: {0}
    WebSocket(#[from] WebSocketServiceError),
    /// App version too old
    AppExpired,
    /// Device deregistered or delinked
    DeviceDeregistered,
    /// Unexpected text frame received
    UnexpectedFrameReceived,
    /// Request message from the server is missing the `id` field
    ServerRequestMissingId,
    /// Failed while sending a request from the server to the incoming  messages channel
    FailedToPassMessageToIncomingChannel,
    /// Failed to decode data received from the server
    IncomingDataInvalid,
    /// Request object must contain only ASCII text as header names and values.
    RequestHasInvalidHeader,
    /// Timeout
    Timeout,
    /// Timed out while establishing connection after {attempts} attempts
    TimeoutEstablishingConnection { attempts: u16 },
    /// All connection routes failed or timed out, {attempts} attempts made
    AllConnectionRoutesFailed { attempts: u16 },
    /// Service is inactive
    ServiceInactive,
    /// Service is unavailable due to the lost connection
    ServiceUnavailable,
    /// Service was disconnected by an intentional local call
    ServiceIntentionallyDisconnected,
    /// Service is unavailable now, try again after {retry_after_seconds}s
    RetryLater { retry_after_seconds: u32 },
}

impl LogSafeDisplay for ChatServiceError {}

impl From<WebSocketServiceConnectError> for ChatServiceError {
    fn from(e: WebSocketServiceConnectError) -> Self {
        match e {
            WebSocketServiceConnectError::Connect(e, _) => match e {
                WebSocketConnectError::Transport(e) => match e {
                    TransportConnectError::InvalidConfiguration => {
                        WebSocketServiceError::Other("invalid configuration")
                    }
                    TransportConnectError::TcpConnectionFailed => {
                        WebSocketServiceError::Other("TCP connection failed")
                    }
                    TransportConnectError::DnsError => WebSocketServiceError::Other("DNS error"),
                    TransportConnectError::SslError(_)
                    | TransportConnectError::SslFailedHandshake(_) => {
                        WebSocketServiceError::Other("TLS failure")
                    }
                    TransportConnectError::CertError => {
                        WebSocketServiceError::Other("failed to load certificates")
                    }
                    TransportConnectError::ProxyProtocol => {
                        WebSocketServiceError::Other("proxy protocol error")
                    }
                    TransportConnectError::ClientAbort => {
                        WebSocketServiceError::Other("client abort error")
                    }
                }
                .into(),
                WebSocketConnectError::Timeout => Self::Timeout,
                WebSocketConnectError::WebSocketError(e) => Self::WebSocket(e.into()),
            },
            WebSocketServiceConnectError::RejectedByServer {
                response,
                received_at: _,
            } => {
                // Retry-After takes precedence over everything else.
                if let Some(retry_after_seconds) = extract_retry_after_seconds(response.headers()) {
                    return Self::RetryLater {
                        retry_after_seconds,
                    };
                }
                match response.status().as_u16() {
                    499 => Self::AppExpired,
                    403 => {
                        // Technically this only applies to identified sockets,
                        // but unidentified sockets should never produce a 403 anyway.
                        Self::DeviceDeregistered
                    }
                    _ => Self::WebSocket(WebSocketServiceError::Http(response)),
                }
            }
        }
    }
}

impl<E: LogSafeDisplay + Into<ChatServiceError>> From<service::ConnectError<E>>
    for ChatServiceError
{
    fn from(e: service::ConnectError<E>) -> Self {
        match e {
            service::ConnectError::Timeout { attempts } => {
                Self::TimeoutEstablishingConnection { attempts }
            }
            service::ConnectError::AllRoutesFailed { attempts } => {
                Self::AllConnectionRoutesFailed { attempts }
            }
            service::ConnectError::RejectedByServer(e) => e.into(),
        }
    }
}

impl From<service::StateError> for ChatServiceError {
    fn from(e: service::StateError) -> Self {
        match e {
            service::StateError::Inactive => Self::ServiceInactive,
            service::StateError::ServiceUnavailable => Self::ServiceUnavailable,
        }
    }
}
