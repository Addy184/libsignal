//
// Copyright 2024 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::time::{Duration, SystemTime};

use base64::prelude::{
    Engine as _, BASE64_STANDARD, BASE64_STANDARD_NO_PAD, BASE64_URL_SAFE_NO_PAD,
};
use futures_util::future::BoxFuture;
use http::header::{ACCEPT, CONTENT_TYPE};
use http::uri::PathAndQuery;
use libsignal_core::{Aci, E164};
use libsignal_keytrans::{
    AccountData, ChatDistinguishedResponse, ChatMonitorResponse, ChatSearchResponse,
    CondensedTreeSearchResponse, FullSearchResponse, FullTreeHead, KeyTransparency, LastTreeHead,
    LocalStateUpdate, MonitorContext, MonitorKey, MonitorProof, MonitorRequest, MonitorResponse,
    MonitoringData, SearchContext, SearchStateUpdate, SlimSearchRequest, StoredAccountData,
    StoredMonitoringData, StoredTreeHead, VerifiedSearchResult,
};
use libsignal_protocol::{IdentityKey, PublicKey};
use prost::{DecodeError, Message};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chat;

const SEARCH_PATH: &str = "/v1/key-transparency/search";
const DISTINGUISHED_PATH: &str = "/v1/key-transparency/distinguished";
const MONITOR_PATH: &str = "/v1/key-transparency/monitor";

const MIME_TYPE: &str = "application/json";

fn common_headers() -> http::HeaderMap {
    http::HeaderMap::from_iter([
        (CONTENT_TYPE, http::HeaderValue::from_static(MIME_TYPE)),
        (ACCEPT, http::HeaderValue::from_static(MIME_TYPE)),
    ])
}

#[derive(Debug, Error, displaydoc::Display)]
pub enum Error {
    /// Chat request failed: {0}
    ChatServiceError(#[from] chat::ChatServiceError),
    /// Bad status code: {0}
    RequestFailed(http::StatusCode),
    /// Verification failed: {0}
    VerificationFailed(#[from] libsignal_keytrans::Error),
    /// Invalid response: {0}
    InvalidResponse(&'static str),
    /// Invalid request: {0}
    InvalidRequest(&'static str),
    /// Invalid protobuf: {0}
    DecodingFailed(DecodeError),
}

impl From<DecodeError> for Error {
    fn from(err: DecodeError) -> Self {
        Error::DecodingFailed(err)
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawChatSearchRequest {
    aci: String,
    aci_identity_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    e164: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unidentified_access_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_tree_head_size: Option<u64>,
    distinguished_tree_head_size: u64,
}

impl RawChatSearchRequest {
    fn new(
        aci: &Aci,
        aci_identity_key: &PublicKey,
        e164: Option<&(E164, Vec<u8>)>,
        username_hash: Option<&UsernameHash>,
        last_tree_head_size: Option<u64>,
        distinguished_tree_head_size: u64,
    ) -> Self {
        Self {
            aci: aci.as_chat_value(),
            aci_identity_key: BASE64_STANDARD.encode(aci_identity_key.serialize()),
            e164: e164.map(|x| x.0.as_chat_value()),
            username_hash: username_hash.map(|x| x.as_chat_value()),
            unidentified_access_key: e164.map(|x| BASE64_STANDARD.encode(&x.1)),
            last_tree_head_size,
            distinguished_tree_head_size,
        }
    }
}

impl From<RawChatSearchRequest> for chat::Request {
    fn from(request: RawChatSearchRequest) -> Self {
        Self {
            method: http::Method::POST,
            body: Some(serde_json::to_vec(&request).unwrap().into_boxed_slice()),
            headers: common_headers(),
            path: PathAndQuery::from_static(SEARCH_PATH),
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RawChatSerializedResponse {
    serialized_response: String,
}

impl TryFrom<chat::Response> for RawChatSerializedResponse {
    type Error = Error;

    fn try_from(response: chat::Response) -> Result<Self> {
        let body = response
            .body
            .ok_or(Error::InvalidResponse("missing body"))?;
        serde_json::from_slice(&body).map_err(|_| Error::InvalidResponse("invalid JSON"))
    }
}

// Differs from [`ChatSearchResponse`] by establishing proper optionality of fields.
struct TypedSearchResponse {
    full_tree_head: FullTreeHead,
    aci_search_response: CondensedTreeSearchResponse,
    e164_search_response: Option<CondensedTreeSearchResponse>,
    username_hash_search_response: Option<CondensedTreeSearchResponse>,
}

impl TypedSearchResponse {
    fn from_untyped(
        require_e164: bool,
        require_username_hash: bool,
        response: ChatSearchResponse,
    ) -> Result<Self> {
        if require_e164 != response.e164.is_some()
            || require_username_hash != response.username_hash.is_some()
        {
            return Err(Error::InvalidResponse(
                "request/response optionality mismatch",
            ));
        }
        let ChatSearchResponse {
            tree_head,
            aci,
            e164,
            username_hash,
        } = response;
        Ok(Self {
            full_tree_head: tree_head.ok_or(Error::InvalidResponse("missing tree head"))?,
            aci_search_response: aci
                .ok_or(Error::InvalidResponse("missing ACI search response"))?,
            e164_search_response: e164,
            username_hash_search_response: username_hash,
        })
    }
}

fn decode_response<S, R>(b64: S) -> Result<R>
where
    S: AsRef<str>,
    R: Message + Default,
{
    let proto_bytes = BASE64_STANDARD_NO_PAD
        .decode(b64.as_ref())
        .map_err(|_| Error::InvalidResponse("invalid base64"))?;

    R::decode(proto_bytes.as_slice())
        .map_err(|_| Error::InvalidResponse("invalid search response protobuf encoding"))
}

// 0x00 is the current version prefix
const SEARCH_VALUE_PREFIX: u8 = 0x00;

/// A safe-to-use wrapper around the values returned by KT server.
///
/// The KT server stores values prefixed with an extra "version" byte, that needs
/// to be stripped.
///
/// SearchValue validates the prefix upon construction from raw bytes, and
/// provides access to the actual underlying value via its payload method.
struct SearchValue<'a> {
    raw: &'a [u8],
}

impl<'a> TryFrom<&'a VerifiedSearchResult> for SearchValue<'a> {
    type Error = Error;

    fn try_from(result: &'a VerifiedSearchResult) -> Result<Self> {
        let raw = result.value.as_slice();
        if raw.first() == Some(&SEARCH_VALUE_PREFIX) {
            Ok(Self { raw })
        } else {
            Err(Error::InvalidResponse("bad value format"))
        }
    }
}

impl SearchValue<'_> {
    fn payload(&self) -> &[u8] {
        &self.raw[1..]
    }
}

impl TryFrom<SearchValue<'_>> for Aci {
    type Error = Error;

    fn try_from(value: SearchValue) -> std::result::Result<Self, Self::Error> {
        Aci::parse_from_service_id_binary(value.payload()).ok_or(Error::InvalidResponse("bad ACI"))
    }
}

impl TryFrom<SearchValue<'_>> for IdentityKey {
    type Error = Error;

    fn try_from(value: SearchValue) -> std::result::Result<Self, Self::Error> {
        IdentityKey::decode(value.payload()).map_err(|_| Error::InvalidResponse("bad identity key"))
    }
}

struct RawChatDistinguishedRequest {
    last_tree_head_size: Option<u64>,
}

impl From<RawChatDistinguishedRequest> for chat::Request {
    fn from(request: RawChatDistinguishedRequest) -> Self {
        let query_string = request
            .last_tree_head_size
            .map(|n| format!("lastTreeHeadSize={n}"))
            .unwrap_or_default();
        let path_and_query = PathAndQuery::try_from(format!("{DISTINGUISHED_PATH}?{query_string}"))
            .expect("valid path and query");
        Self {
            method: http::Method::GET,
            body: None,
            headers: common_headers(),
            path: path_and_query,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueMonitor {
    value: String,
    entry_position: u64,
    commitment_index: String,
}

impl ValueMonitor {
    fn new(value: String, entry_position: u64, commitment_index: &[u8]) -> Self {
        Self {
            value,
            entry_position,
            commitment_index: BASE64_STANDARD_NO_PAD.encode(commitment_index),
        }
    }

    fn for_aci(aci: &Aci, entry_position: u64, commitment_index: &[u8]) -> Self {
        Self::new(aci.as_chat_value(), entry_position, commitment_index)
    }

    fn for_e164(e164: E164, entry_position: u64, commitment_index: &[u8]) -> Self {
        Self::new(e164.as_chat_value(), entry_position, commitment_index)
    }

    fn for_username_hash(
        username_hash: &UsernameHash,
        entry_position: u64,
        commitment_index: &[u8],
    ) -> Self {
        Self::new(
            username_hash.as_chat_value(),
            entry_position,
            commitment_index,
        )
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawChatMonitorRequest {
    aci: ValueMonitor,
    #[serde(skip_serializing_if = "Option::is_none")]
    e164: Option<ValueMonitor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username_hash: Option<ValueMonitor>,
    last_non_distinguished_tree_head_size: u64,
    last_distinguished_tree_head_size: u64,
}

impl From<RawChatMonitorRequest> for chat::Request {
    fn from(request: RawChatMonitorRequest) -> Self {
        Self {
            method: http::Method::POST,
            body: Some(serde_json::to_vec(&request).unwrap().into_boxed_slice()),
            headers: common_headers(),
            path: PathAndQuery::from_static(MONITOR_PATH),
        }
    }
}

impl RawChatMonitorRequest {
    fn new(
        aci: &Aci,
        e164: Option<E164>,
        username_hash: &Option<UsernameHash<'_>>,
        account_data: &AccountData,
        distinguished_tree_head_size: u64,
    ) -> Result<Self> {
        let last_non_distinguished_tree_head_size = account_data.last_tree_head.0.tree_size;

        if e164.is_some() != account_data.e164.is_some()
            || username_hash.is_some() != account_data.username_hash.is_some()
        {
            return Err(Error::InvalidRequest(
                "account data does not match the monitor request",
            ));
        }

        Ok(Self {
            aci: ValueMonitor::for_aci(
                aci,
                account_data.aci.latest_log_position(),
                &account_data.aci.index,
            ),
            e164: e164.map(|e164| {
                ValueMonitor::for_e164(
                    e164,
                    account_data.e164.as_ref().unwrap().latest_log_position(),
                    &account_data.e164.as_ref().unwrap().index,
                )
            }),
            username_hash: username_hash.as_ref().map(|unh| {
                ValueMonitor::for_username_hash(
                    unh,
                    account_data
                        .username_hash
                        .as_ref()
                        .unwrap()
                        .latest_log_position(),
                    &account_data.username_hash.as_ref().unwrap().index,
                )
            }),
            last_non_distinguished_tree_head_size,
            last_distinguished_tree_head_size: distinguished_tree_head_size,
        })
    }
}

// Same as ChatMonitorResponse, only with the right optionality of fields
#[derive(Clone, Debug)]
struct TypedMonitorResponse {
    tree_head: FullTreeHead,
    aci: MonitorProof,
    e164: Option<MonitorProof>,
    username_hash: Option<MonitorProof>,
    inclusion: Vec<Vec<u8>>,
}

impl TypedMonitorResponse {
    fn from_untyped(
        require_e164: bool,
        require_username_hash: bool,
        response: ChatMonitorResponse,
    ) -> Result<Self> {
        if require_e164 != response.e164.is_some()
            || require_username_hash != response.username_hash.is_some()
        {
            return Err(Error::InvalidResponse(
                "request/response optionality mismatch",
            ));
        }
        let ChatMonitorResponse {
            tree_head,
            aci,
            username_hash,
            e164,
            inclusion,
        } = response;
        Ok(Self {
            tree_head: tree_head.ok_or(Error::InvalidResponse("missing tree head"))?,
            aci: aci.ok_or(Error::InvalidResponse("missing ACI monitor proof"))?,
            e164,
            username_hash,
            inclusion,
        })
    }
}

pub trait UnauthenticatedChat {
    fn send_unauthenticated(
        &self,
        request: chat::Request,
        timeout: Duration,
    ) -> BoxFuture<'_, std::result::Result<chat::Response, chat::ChatServiceError>>;
}

pub struct Config {
    chat_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chat_timeout: Duration::from_secs(10),
        }
    }
}

pub struct Kt<'a> {
    pub inner: KeyTransparency,
    pub chat: &'a (dyn UnauthenticatedChat + Sync),
    pub config: Config,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub aci_identity_key: IdentityKey,
    pub aci_for_e164: Option<Aci>,
    pub aci_for_username_hash: Option<Aci>,
    pub timestamp: SystemTime,
    pub account_data: StoredAccountData,
}

impl<'a> Kt<'a> {
    async fn send(&self, request: chat::Request) -> Result<chat::Response> {
        log::debug!("{}", &request.path.as_str());
        log::debug!(
            "{}",
            &String::from_utf8(request.clone().body.unwrap_or_default().to_vec()).unwrap()
        );
        let response = self
            .chat
            .send_unauthenticated(request, self.config.chat_timeout)
            .await?;
        log::debug!(
            "{} {:?}, headers: {:?}, body: {}",
            &response.status,
            &response.message,
            &response.headers,
            hex::encode({
                let body_slice = response.body.as_deref().unwrap_or_default();
                &body_slice[..body_slice.len().min(1024)]
            })
        );
        if !response.status.is_success() {
            Err(Error::RequestFailed(response.status))
        } else {
            Ok(response)
        }
    }

    pub async fn search(
        &self,
        aci: &Aci,
        aci_identity_key: &PublicKey,
        e164: Option<(E164, Vec<u8>)>,
        username_hash: Option<UsernameHash<'a>>,
        stored_account_data: Option<AccountData>,
        distinguished_tree_head: &LastTreeHead,
    ) -> Result<SearchResult> {
        let raw_request = RawChatSearchRequest::new(
            aci,
            aci_identity_key,
            e164.as_ref(),
            username_hash.as_ref(),
            stored_account_data
                .as_ref()
                .map(|acc_data| acc_data.last_tree_head.0.tree_size),
            distinguished_tree_head.0.tree_size,
        );
        let response = self.send(raw_request.into()).await?;

        let chat_search_response = RawChatSerializedResponse::try_from(response)
            .and_then(|r| decode_response(r.serialized_response))
            .and_then(|r| {
                TypedSearchResponse::from_untyped(e164.is_some(), username_hash.is_some(), r)
            })?;

        let now = SystemTime::now();

        verify_chat_search_response(
            &self.inner,
            aci,
            e164.map(|(e164, _)| e164),
            username_hash,
            stored_account_data,
            chat_search_response,
            Some(distinguished_tree_head),
            now,
        )
    }

    pub async fn distinguished(
        &self,
        last_distinguished: Option<LastTreeHead>,
    ) -> Result<SearchStateUpdate> {
        let distinguished_size = last_distinguished
            .as_ref()
            .map(|last_tree_head| last_tree_head.0.tree_size);

        let raw_request = RawChatDistinguishedRequest {
            last_tree_head_size: distinguished_size,
        };
        let response = self.send(raw_request.into()).await?;

        let ChatDistinguishedResponse {
            tree_head,
            distinguished,
        } = RawChatSerializedResponse::try_from(response)
            .and_then(|r| decode_response(r.serialized_response))?;

        let tree_head = tree_head.ok_or(Error::InvalidResponse("tree head must be present"))?;
        let condensed_response =
            distinguished.ok_or(Error::InvalidResponse("search response must be present"))?;
        let search_response = FullSearchResponse::new(condensed_response, &tree_head);

        let slim_search_request = SlimSearchRequest::new(b"distinguished".to_vec());

        let verified_result = self.inner.verify_search(
            slim_search_request,
            search_response,
            SearchContext {
                last_tree_head: None,
                last_distinguished_tree_head: last_distinguished.as_ref(),
                data: None,
            },
            false,
            SystemTime::now(),
        )?;
        Ok(verified_result.state_update)
    }

    pub async fn monitor(
        &self,
        aci: Aci,
        e164: Option<E164>,
        username_hash: Option<UsernameHash<'a>>,
        account_data: AccountData,
        last_distinguished_tree_head: &LastTreeHead,
    ) -> Result<AccountData> {
        let raw_request = RawChatMonitorRequest::new(
            &aci,
            e164,
            &username_hash,
            &account_data,
            last_distinguished_tree_head.0.tree_size,
        )?;
        let response = self.send(raw_request.into()).await?;

        let chat_monitor_response = RawChatSerializedResponse::try_from(response)
            .and_then(|r| decode_response(r.serialized_response))
            .and_then(|r| {
                TypedMonitorResponse::from_untyped(e164.is_some(), username_hash.is_some(), r)
            })?;

        let now = SystemTime::now();

        let updated_account_data = {
            let AccountData {
                aci: aci_monitoring_data,
                e164: e164_monitoring_data,
                username_hash: username_hash_monitoring_data,
                last_tree_head,
            } = account_data;

            let mut monitor_keys = Vec::with_capacity(3);
            let mut proofs = Vec::with_capacity(3);
            let mut monitoring_data_map = HashMap::with_capacity(3);

            let aci_monitor_key = MonitorKey {
                search_key: aci.as_search_key(),
                entry_position: aci_monitoring_data.latest_log_position(),
                commitment_index: aci_monitoring_data.index.to_vec(),
            };
            monitor_keys.push(aci_monitor_key);
            proofs.push(chat_monitor_response.aci);
            monitoring_data_map.insert(aci.as_search_key(), aci_monitoring_data.clone());

            if let Some(e164) = e164 {
                let monitoring_data = e164_monitoring_data
                    .ok_or(Error::InvalidRequest("missing E.164 monitoring data"))?;
                let key = MonitorKey {
                    search_key: e164.as_search_key(),
                    entry_position: monitoring_data.latest_log_position(),
                    commitment_index: monitoring_data.index.to_vec(),
                };
                monitor_keys.push(key);

                // The proof must be present. Checked in TypedMonitorResponse::from_untyped
                proofs.push(chat_monitor_response.e164.unwrap());
                monitoring_data_map.insert(e164.as_search_key(), monitoring_data);
            }

            if let Some(username_hash) = username_hash.clone() {
                let monitoring_data = username_hash_monitoring_data.ok_or(
                    Error::InvalidRequest("missing username hash monitoring data"),
                )?;
                let key = MonitorKey {
                    search_key: username_hash.as_search_key().to_vec(),
                    entry_position: monitoring_data.latest_log_position(),
                    commitment_index: monitoring_data.index.to_vec(),
                };
                monitor_keys.push(key);
                // The proof must be present. Checked in TypedMonitorResponse::from_untyped
                proofs.push(chat_monitor_response.username_hash.unwrap());
                monitoring_data_map.insert(username_hash.as_search_key(), monitoring_data);
            }

            // We are using a single monitor request/response pair for all the possible keys
            let monitor_request = MonitorRequest {
                keys: monitor_keys,
                // Consistency is only used to verify "distinguished" search key
                consistency: None,
            };

            let monitor_response = MonitorResponse {
                tree_head: Some(chat_monitor_response.tree_head.clone()),
                proofs,
                inclusion: chat_monitor_response.inclusion,
            };

            let monitor_context = MonitorContext {
                last_tree_head: Some(&last_tree_head),
                last_distinguished_tree_head,
                data: monitoring_data_map,
            };

            let verified = self.inner.verify_monitor(
                &monitor_request,
                &monitor_response,
                monitor_context,
                now,
            )?;

            let LocalStateUpdate {
                tree_head,
                tree_root,
                mut monitoring_data,
            } = verified;

            let mut take_data = move |search_key: &[u8], err_message: &'static str| {
                monitoring_data
                    .remove(search_key)
                    .ok_or(Error::InvalidResponse(err_message))
            };

            AccountData {
                aci: take_data(&aci.as_search_key(), "ACI monitoring data is missing")?,
                e164: e164
                    .map(|e164| {
                        take_data(&e164.as_search_key(), "E.164 monitoring data is missing")
                    })
                    .transpose()?,
                username_hash: username_hash
                    .map(|username_hash| {
                        take_data(
                            &username_hash.as_search_key(),
                            "username hash monitoring data is missing",
                        )
                    })
                    .transpose()?,
                last_tree_head: (tree_head, tree_root),
            }
        };

        Ok(updated_account_data)
    }
}

fn verify_single_search_response(
    kt: &KeyTransparency,
    search_key: Vec<u8>,
    response: CondensedTreeSearchResponse,
    monitoring_data: Option<MonitoringData>,
    full_tree_head: &FullTreeHead,
    last_tree_head: Option<&LastTreeHead>,
    last_distinguished_tree_head: Option<&LastTreeHead>,
    now: SystemTime,
) -> Result<VerifiedSearchResult> {
    let result = kt.verify_search(
        SlimSearchRequest::new(search_key),
        FullSearchResponse::new(response, full_tree_head),
        SearchContext {
            last_tree_head,
            last_distinguished_tree_head,
            data: monitoring_data.map(MonitoringData::from),
        },
        true,
        now,
    )?;
    Ok(result)
}

fn verify_chat_search_response(
    kt: &KeyTransparency,
    aci: &Aci,
    e164: Option<E164>,
    username_hash: Option<UsernameHash>,
    stored_account_data: Option<AccountData>,
    chat_search_response: TypedSearchResponse,
    last_distinguished_tree_head: Option<&LastTreeHead>,
    now: SystemTime,
) -> Result<SearchResult> {
    let TypedSearchResponse {
        full_tree_head,
        aci_search_response,
        e164_search_response,
        username_hash_search_response,
    } = chat_search_response;

    let (
        aci_monitoring_data,
        e164_monitoring_data,
        username_hash_monitoring_data,
        stored_last_tree_head,
    ) = match stored_account_data {
        None => (None, None, None, None),
        Some(acc) => {
            let AccountData {
                aci,
                e164,
                username_hash,
                last_tree_head,
            } = acc;
            (Some(aci), e164, username_hash, Some(last_tree_head))
        }
    };

    let aci_result = verify_single_search_response(
        kt,
        aci.as_search_key(),
        aci_search_response,
        aci_monitoring_data,
        &full_tree_head,
        stored_last_tree_head.as_ref(),
        last_distinguished_tree_head,
        now,
    )?;

    let e164_result = both_or_neither(
        e164,
        e164_search_response,
        "E.164 request/response mismatch",
    )?
    .map(|(e164, e164_search_response)| {
        verify_single_search_response(
            kt,
            e164.as_search_key(),
            e164_search_response,
            e164_monitoring_data,
            &full_tree_head,
            stored_last_tree_head.as_ref(),
            last_distinguished_tree_head,
            now,
        )
    })
    .transpose()?;

    let username_hash_result = both_or_neither(
        username_hash,
        username_hash_search_response,
        "Username hash request/response mismatch",
    )?
    .map(|(username_hash, username_hash_response)| {
        verify_single_search_response(
            kt,
            username_hash.as_search_key(),
            username_hash_response,
            username_hash_monitoring_data,
            &full_tree_head,
            stored_last_tree_head.as_ref(),
            last_distinguished_tree_head,
            now,
        )
    })
    .transpose()?;

    if !aci_result.are_all_roots_equal([e164_result.as_ref(), username_hash_result.as_ref()]) {
        return Err(Error::InvalidResponse("mismatching tree roots"));
    }

    let identity_key = extract_value_as::<IdentityKey>(&aci_result)?;
    let aci_for_e164 = e164_result
        .as_ref()
        .map(extract_value_as::<Aci>)
        .transpose()?;
    let aci_for_username_hash = username_hash_result
        .as_ref()
        .map(extract_value_as::<Aci>)
        .transpose()?;

    // ACI response is guaranteed to be present, taking the last tree head from it.
    let LocalStateUpdate {
        tree_head,
        tree_root,
        monitoring_data: updated_aci_monitoring_data,
    } = aci_result.state_update;

    let last_tree_head = StoredTreeHead {
        tree_head: Some(tree_head),
        root: tree_root.into(),
    };

    let updated_account_data = StoredAccountData {
        aci: updated_aci_monitoring_data.map(StoredMonitoringData::from),
        e164: e164_result
            .and_then(|r| r.state_update.monitoring_data)
            .map(StoredMonitoringData::from),
        username_hash: username_hash_result
            .and_then(|r| r.state_update.monitoring_data)
            .map(StoredMonitoringData::from),
        last_tree_head: Some(last_tree_head),
    };

    Ok(SearchResult {
        aci_identity_key: identity_key,
        aci_for_e164,
        aci_for_username_hash,
        timestamp: now,
        account_data: updated_account_data,
    })
}

fn both_or_neither<T, U>(a: Option<T>, b: Option<U>, msg: &'static str) -> Result<Option<(T, U)>> {
    match (a, b) {
        (Some(a), Some(b)) => Ok(Some((a, b))),
        (None, None) => Ok(None),
        (None, Some(_)) | (Some(_), None) => Err(Error::InvalidResponse(msg)),
    }
}

// Cannot be a method on VerifiedSearchResult due to use of SearchValue
fn extract_value_as<T>(result: &VerifiedSearchResult) -> Result<T>
where
    T: for<'a> TryFrom<SearchValue<'a>, Error = Error>,
{
    let val = SearchValue::try_from(result)?;
    val.try_into()
}

const SEARCH_KEY_PREFIX_ACI: &[u8] = b"a";
const SEARCH_KEY_PREFIX_E164: &[u8] = b"n";
const SEARCH_KEY_PREFIX_USERNAME_HASH: &[u8] = b"u";

/// Representation of an object as "search key" aligned with conversion
/// performed by the chat server.
///
/// Search keys from the Key Transparency server perspective are just arrays of
/// bytes, therefore in order to distinguish them and avoid (highly unlikely)
/// clashes Chat server adds unique prefixes to keys representing ACIs, E.164's,
/// and username hashes.
pub trait SearchKey {
    fn as_search_key(&self) -> Vec<u8>;
}

impl SearchKey for Aci {
    fn as_search_key(&self) -> Vec<u8> {
        [SEARCH_KEY_PREFIX_ACI, self.service_id_binary().as_slice()].concat()
    }
}

impl SearchKey for E164 {
    fn as_search_key(&self) -> Vec<u8> {
        [SEARCH_KEY_PREFIX_E164, self.to_string().as_bytes()].concat()
    }
}

/// Type-safe wrapper for a byte slice representing username hash.
#[derive(Clone)]
pub struct UsernameHash<'a>(Cow<'a, [u8]>);

impl AsRef<[u8]> for UsernameHash<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl Debug for UsernameHash<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsernameHash")
            .field("hex", &hex::encode(self.0.as_ref()))
            .finish()
    }
}

impl<'a> UsernameHash<'a> {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Cow::Owned(bytes))
    }

    pub fn from_slice(bytes: &'a [u8]) -> Self {
        Self(Cow::Borrowed(bytes))
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0.into_owned()
    }
}

impl From<Vec<u8>> for UsernameHash<'_> {
    fn from(vec: Vec<u8>) -> Self {
        Self(Cow::Owned(vec))
    }
}

impl From<Box<[u8]>> for UsernameHash<'_> {
    fn from(value: Box<[u8]>) -> Self {
        Self(Cow::Owned(value.into_vec()))
    }
}

impl SearchKey for UsernameHash<'_> {
    fn as_search_key(&self) -> Vec<u8> {
        [SEARCH_KEY_PREFIX_USERNAME_HASH, self.0.as_ref()].concat()
    }
}

/// String representation of a value to be sent in chat server JSON requests.
trait AsChatValue {
    fn as_chat_value(&self) -> String;
}

impl AsChatValue for Aci {
    fn as_chat_value(&self) -> String {
        self.service_id_string()
    }
}

impl AsChatValue for E164 {
    fn as_chat_value(&self) -> String {
        self.to_string()
    }
}

impl AsChatValue for UsernameHash<'_> {
    fn as_chat_value(&self) -> String {
        BASE64_URL_SAFE_NO_PAD.encode(self.as_ref())
    }
}

#[cfg(test)]
mod test_support {
    use libsignal_keytrans::{DeploymentMode, PublicConfig, VerifyingKey, VrfPublicKey};

    use super::*;
    use crate::auth::Auth;
    use crate::chat::test_support::AnyChat;
    use crate::env;
    use crate::env::{
        KEYTRANS_AUDITOR_KEY_MATERIAL_STAGING, KEYTRANS_SIGNING_KEY_MATERIAL_STAGING,
        KEYTRANS_VRF_KEY_MATERIAL_STAGING,
    };

    impl UnauthenticatedChat for AnyChat {
        fn send_unauthenticated(
            &self,
            request: chat::Request,
            timeout: Duration,
        ) -> BoxFuture<'_, std::result::Result<chat::Response, chat::ChatServiceError>> {
            Box::pin(self.send_unauthenticated(request, timeout))
        }
    }

    pub(super) mod test_account {
        use hex_literal::hex;
        use libsignal_core::E164;
        use nonzero_ext::nonzero;
        use uuid::Uuid;

        pub const ACI: Uuid = uuid::uuid!("90c979fd-eab4-4a08-b6da-69dedeab9b29");
        pub const ACI_IDENTITY_KEY_BYTES: &[u8] =
            &hex!("05d0e797ec91a4bce0e88959c419e96eb4fdabbb3dc688965584c966dc24195609");
        pub const USERNAME_HASH: &[u8] =
            &hex!("d237a4b83b463ca7da58d4a16bf6a3ba104506eb412b235eb603ea10f467c655");
        pub const PHONE_NUMBER: E164 = E164::new(nonzero!(18005550100u64));
        pub const UNIDENTIFIED_ACCESS_KEY: &[u8] = &hex!("fdc7951d1507268daf1834b74d23b76c");
    }

    pub(super) fn make_key_transparency() -> KeyTransparency {
        let signature_key = VerifyingKey::from_bytes(KEYTRANS_SIGNING_KEY_MATERIAL_STAGING)
            .expect("valid signature key material");
        let vrf_key = VrfPublicKey::try_from(*KEYTRANS_VRF_KEY_MATERIAL_STAGING)
            .expect("valid vrf key material");
        let auditor_key = VerifyingKey::from_bytes(KEYTRANS_AUDITOR_KEY_MATERIAL_STAGING)
            .expect("valid auditor key material");
        KeyTransparency {
            config: PublicConfig {
                mode: DeploymentMode::ThirdPartyAuditing(auditor_key),
                signature_key,
                vrf_key,
            },
        }
    }

    pub(super) fn make_kt(chat: &AnyChat) -> Kt {
        Kt {
            inner: make_key_transparency(),
            chat,
            config: Default::default(),
        }
    }

    pub(super) async fn make_chat() -> AnyChat {
        use crate::chat::test_support::simple_chat_service;
        let chat = simple_chat_service(
            &env::STAGING,
            Auth::default(),
            vec![env::STAGING
                .chat_domain_config
                .connect
                .direct_connection_params()],
        );
        chat.connect_unauthenticated()
            .await
            .expect("can connect to chat");
        chat
    }

    #[allow(dead_code)]
    // This function automates the collection of the test data.
    //
    // In particular, the constants that start with:
    // - DISTINGUISHED_TREE_
    // - STORED_ACCOUNT_DATA_
    //
    // In order to collect the test data:
    // - Uncomment the #[tokio::test] line
    // - Execute the test as `cargo test --package libsignal-net --all-features collect_test_data -- --nocapture`
    // - Follow the prompts
    // - Replace the "const" definitions in the code with the ones printed out by the test.
    // - Copy the "chat_search_response.dat" file to "rust/net/tests/data/" replacing the existing one.
    //
    //#[tokio::test]
    async fn collect_test_data() {
        fn prompt(text: &str) {
            println!("{} >", text);

            let mut input = String::new();

            std::io::stdin()
                .read_line(&mut input)
                .expect("can read_line from stdin");
        }

        let chat = make_chat().await;
        let kt = make_kt(&chat);

        prompt("Let's collect some data (press ENTER)");

        println!("Requesting distinguished tree...");
        let result = kt.distinguished(None).await.expect("can get distinguished");

        let distinguished_tree_size = result.tree_head.tree_size;
        println!("Distinguished tree");
        println!("Size: {}", &result.tree_head.tree_size);
        println!(
            "const DISTINGUISHED_TREE_{}_ROOT: &[u8] = &hex!(\"{}\");",
            distinguished_tree_size,
            &hex::encode(result.tree_head.encode_to_vec())
        );
        println!(
            "const DISTINGUISHED_TREE_{}_HEAD: &[u8] = &hex!(\"{}\");",
            distinguished_tree_size,
            &hex::encode(result.tree_root)
        );

        let distinguished_tree = (result.tree_head, result.tree_root);

        prompt("Now advance the tree (and press ENTER)");

        let aci = Aci::from(test_account::ACI);
        let aci_identity_key =
            PublicKey::deserialize(test_account::ACI_IDENTITY_KEY_BYTES).expect("valid key bytes");
        let e164 = (
            test_account::PHONE_NUMBER,
            test_account::UNIDENTIFIED_ACCESS_KEY.to_vec(),
        );
        let username_hash = UsernameHash(Cow::Borrowed(test_account::USERNAME_HASH));

        println!("Requesting account data...");

        let result = kt
            .search(
                &aci,
                &aci_identity_key,
                Some(e164.clone()),
                Some(username_hash.clone()),
                None,
                &distinguished_tree,
            )
            .await
            .expect("can perform search");

        let last_tree_size = result
            .account_data
            .clone()
            .last_tree_head
            .unwrap()
            .tree_head
            .unwrap()
            .tree_size;

        assert_ne!(
            distinguished_tree_size, last_tree_size,
            "The tree did not advance!"
        );

        println!("Stored account data:");
        println!(
            "const STORED_ACCOUNT_DATA_{}: &[u8] = &hex!(\"{}\");",
            last_tree_size,
            &hex::encode(result.account_data.encode_to_vec())
        );

        let account_data = AccountData::try_from(result.account_data).expect("valid account data");

        prompt("Now advance the tree. Yes, again! (and press ENTER)");

        let raw_request = RawChatSearchRequest::new(
            &aci,
            &aci_identity_key,
            Some(&e164),
            Some(&username_hash),
            Some(account_data.last_tree_head.0.tree_size),
            distinguished_tree.0.tree_size,
        );
        let response = kt
            .send(raw_request.into())
            .await
            .expect("can send raw search request");

        let raw_response = RawChatSerializedResponse::try_from(response).expect("valid response");
        let response_bytes = BASE64_STANDARD_NO_PAD
            .decode(raw_response.serialized_response.as_bytes())
            .expect("valid base64");

        {
            let search_response = ChatSearchResponse::decode(response_bytes.as_ref())
                .map_err(|_| Error::InvalidResponse("bad protobuf"))
                .and_then(|r| TypedSearchResponse::from_untyped(true, true, r))
                .expect("valid search response");

            let tree_size = search_response.full_tree_head.tree_head.unwrap().tree_size;
            assert_ne!(last_tree_size, tree_size, "The tree did not advance!");
        }

        println!(
            "const CHAT_SEARCH_RESPONSE_VALID_AT: Duration = Duration::from_secs({});",
            SystemTime::UNIX_EPOCH.elapsed().unwrap().as_secs()
        );

        const PATH: &str = "/tmp/chat_search_response.dat";
        println!("Response written to '{PATH}'");
        std::fs::write(PATH, &response_bytes).unwrap()
    }
}

#[cfg(test)]
mod test {
    use std::cmp::Ordering;

    use assert_matches::assert_matches;
    use hex_literal::hex;
    use libsignal_keytrans::TreeHead;
    use test_case::test_case;

    use super::test_support::{make_chat, make_key_transparency, make_kt, test_account};
    use super::*;

    // Distinguished tree parameters as of size 608
    const DISTINGUISHED_TREE_637_ROOT: &[u8] =
        &hex!("fda58cc9c00d4e6734047f98b4723804383f9e64daa7224bdd7591df9276cbb4");
    const DISTINGUISHED_TREE_637_HEAD: &[u8] =
        &hex!("08fd0410f1a3a8cccb321a407761dac20002f5a15b789418d77fe482ec3bdf782a336ecf4f12cbe43ef35fa86360ffcb884354d9854a970afbbf6db716765e3a72fa36b9428918993a8ef30c");
    // Stored account data as of size 611
    const STORED_ACCOUNT_DATA_642: &[u8] =
        &hex!("0a2b0a203901c94081c4e6321e92b3e434dcaf788f5326913e7bdcab47b4fd2ae7a6848a10231a0308ff032001122c0a2086052cc2a2689558e852d053c5ab411d8c3baef20171ec298e551574806ca95d1081011a0308ff0320011a2c0a20bc1cfaae736c27c437b99175798933ee32caf07a5226840ec963a4e614916e9010dc011a0308ff03200122700a4c08820510fbd0d8cdcb321a4041ed17cdfdae313856d8bd6028936f0a2c1494968eafbea1498e2fc666105d9ddbaf7d4e43d9013a713ba58f402557ec794c441ed3bcfacc6bc6d656ea0fcf01122010763b0de052335c451c9bb7b46f52d0eeb736ee9731c4ba6a6f93d74a89cc3b");

    fn test_distinguished_tree() -> LastTreeHead {
        (
            TreeHead::decode(DISTINGUISHED_TREE_637_HEAD).expect("valid TreeHead"),
            DISTINGUISHED_TREE_637_ROOT
                .try_into()
                .expect("valid root size"),
        )
    }

    fn test_account_data() -> StoredAccountData {
        StoredAccountData::decode(STORED_ACCOUNT_DATA_642).expect("valid stored acc data")
    }

    #[tokio::test]
    #[test_case(false, false; "ACI")]
    #[test_case(true, false; "ACI + E164")]
    #[test_case(false, true; "ACI + Username Hash")]
    #[test_case(true, true; "ACI + E164 + Username Hash")]
    async fn search_permutations_integration_test(use_e164: bool, use_username_hash: bool) {
        if std::env::var("LIBSIGNAL_TESTING_RUN_NONHERMETIC_TESTS").is_err() {
            println!("SKIPPED: running integration tests is not enabled");
            return;
        }
        let chat = make_chat().await;
        let kt = make_kt(&chat);

        let aci = Aci::from(test_account::ACI);
        let aci_identity_key =
            PublicKey::deserialize(test_account::ACI_IDENTITY_KEY_BYTES).expect("valid key bytes");
        let e164 = (
            test_account::PHONE_NUMBER,
            test_account::UNIDENTIFIED_ACCESS_KEY.to_vec(),
        );
        let username_hash = UsernameHash(Cow::Borrowed(test_account::USERNAME_HASH));

        let acc_data = AccountData::try_from(test_account_data()).expect("valid acc data");

        let result = kt
            .search(
                &aci,
                &aci_identity_key,
                use_e164.then_some(e164),
                use_username_hash.then_some(username_hash),
                Some(acc_data),
                &test_distinguished_tree(),
            )
            .await
            .expect("can perform search");

        assert_eq!(
            &hex::encode(test_account::ACI_IDENTITY_KEY_BYTES),
            &hex::encode(result.aci_identity_key.serialize())
        );
    }

    #[tokio::test]
    #[test_case(false; "unknown_distinguished")]
    #[test_case(true; "known_distinguished")]
    async fn distinguished_integration_test(have_last_distinguished: bool) {
        if std::env::var("LIBSIGNAL_TESTING_RUN_NONHERMETIC_TESTS").is_err() {
            println!("SKIPPED: running integration tests is not enabled");
            return;
        }

        let chat = make_chat().await;
        let kt = make_kt(&chat);

        let result = kt
            .distinguished(have_last_distinguished.then_some(test_distinguished_tree()))
            .await;

        assert_matches!(result, Ok( LocalStateUpdate {tree_head, ..}) => assert_ne!(tree_head.tree_size, 0));
    }

    #[tokio::test]
    #[test_case(false, false; "ACI")]
    #[test_case(true, false; "ACI + E164")]
    #[test_case(false, true; "ACI + Username Hash")]
    #[test_case(true, true; "ACI + E164 + Username Hash")]
    async fn monitor_permutations_integration_test(use_e164: bool, use_username_hash: bool) {
        if std::env::var("LIBSIGNAL_TESTING_RUN_NONHERMETIC_TESTS").is_err() {
            println!("SKIPPED: running integration tests is not enabled");
            return;
        }
        let chat = make_chat().await;
        let kt = make_kt(&chat);

        let aci = Aci::from(test_account::ACI);
        let e164 = test_account::PHONE_NUMBER;
        let username_hash = UsernameHash(Cow::Borrowed(test_account::USERNAME_HASH));

        let account_data = {
            let mut data =
                AccountData::try_from(test_account_data()).expect("valid stored account data");
            if !use_e164 {
                data.e164 = None;
            }
            if !use_username_hash {
                data.username_hash = None;
            }
            data
        };

        let updated_account_data = kt
            .monitor(
                aci,
                use_e164.then_some(e164),
                use_username_hash.then_some(username_hash),
                account_data.clone(),
                &test_distinguished_tree(),
            )
            .await
            .expect("can monitor");

        match Ord::cmp(
            &updated_account_data.last_tree_head.0.tree_size,
            &account_data.last_tree_head.0.tree_size,
        ) {
            Ordering::Less => panic!("The tree is shrinking"),
            Ordering::Equal => assert_eq!(&updated_account_data, &account_data),
            Ordering::Greater => {
                // verify that the initial position of the ACI in the tree has not changed, at least
                assert_eq!(&updated_account_data.aci.pos, &account_data.aci.pos)
            }
        }
    }

    const CHAT_SEARCH_RESPONSE: &[u8] = include_bytes!("../tests/data/chat_search_response.dat");
    const CHAT_SEARCH_RESPONSE_VALID_AT: Duration = Duration::from_secs(1738283179);

    fn test_search_response() -> TypedSearchResponse {
        let chat_search_response =
            libsignal_keytrans::ChatSearchResponse::decode(CHAT_SEARCH_RESPONSE)
                .expect("valid response");
        TypedSearchResponse::from_untyped(true, true, chat_search_response)
            .expect("valid typed search response")
    }

    enum Skip {
        E164,
        UsernameHash,
        Both,
    }

    #[test_case(Skip::E164; "e164")]
    #[test_case(Skip::UsernameHash; "username_hash")]
    #[test_case(Skip::Both; "e164 + username_hash")]
    fn search_returns_data_not_requested(skip: Skip) {
        let valid_at = SystemTime::UNIX_EPOCH + CHAT_SEARCH_RESPONSE_VALID_AT;

        let aci = Aci::from(test_account::ACI);
        let e164 = test_account::PHONE_NUMBER;
        let username_hash = UsernameHash(Cow::Borrowed(test_account::USERNAME_HASH));

        let kt_impl = make_key_transparency();

        let (e164, username_hash) = match skip {
            Skip::E164 => (None, Some(username_hash)),
            Skip::UsernameHash => (Some(e164), None),
            Skip::Both => (None, None),
        };

        let account_data =
            AccountData::try_from(test_account_data()).expect("valid stored account data");

        let result = verify_chat_search_response(
            &kt_impl,
            &aci,
            e164,
            username_hash,
            Some(account_data),
            test_search_response(),
            Some(&test_distinguished_tree()),
            valid_at,
        );

        assert_matches!(result, Err(Error::InvalidResponse(_)))
    }

    #[test_case(Skip::E164; "e164")]
    #[test_case(Skip::UsernameHash; "username_hash")]
    #[test_case(Skip::Both; "e164 + username_hash")]
    fn optionality_mismatch_in_search_is_an_error(skip: Skip) {
        let valid_at = SystemTime::UNIX_EPOCH + CHAT_SEARCH_RESPONSE_VALID_AT;

        let aci = Aci::from(test_account::ACI);
        let e164 = test_account::PHONE_NUMBER;
        let username_hash = UsernameHash(Cow::Borrowed(test_account::USERNAME_HASH));

        let kt_impl = make_key_transparency();
        let mut search_response = test_search_response();
        match skip {
            Skip::E164 => search_response.e164_search_response = None,
            Skip::UsernameHash => search_response.username_hash_search_response = None,
            Skip::Both => {
                search_response.e164_search_response = None;
                search_response.username_hash_search_response = None;
            }
        }

        let account_data =
            AccountData::try_from(test_account_data()).expect("valid stored account data");

        let result = verify_chat_search_response(
            &kt_impl,
            &aci,
            Some(e164),
            Some(username_hash),
            Some(account_data),
            search_response,
            Some(&test_distinguished_tree()),
            valid_at,
        );

        assert_matches!(result, Err(Error::InvalidResponse(_)));
    }

    enum ConsistencyMode {
        /// Account data was not in the request,
        /// but response has last consistency proof
        UnexpectedLast,
        /// Last distinguished tree size was not provided,
        /// but response has distinguished consistency proof
        UnexpectedDistinguished,
        /// Account data was provided,
        /// but last consistency proof is missing from response
        EmptyLast,
        /// Last distinguished tree size was provided,
        /// but distinguished consistency proof is missing from response
        EmptyDistinguished,

        /// Only last consistency proof is expected and is present
        OnlyLast,
        /// Only distinguished consistency proof is expected and is present
        OnlyDistinguished,
        /// Both last and distinguished consistency proofs are expected and present
        LastAndDistinguished,
        /// Both last and distinguished are not expected and proofs are empty
        NeitherLastNorDistinguished,
    }

    impl ConsistencyMode {
        fn is_failure(&self) -> bool {
            match self {
                ConsistencyMode::UnexpectedLast
                | ConsistencyMode::UnexpectedDistinguished
                | ConsistencyMode::EmptyLast
                | ConsistencyMode::EmptyDistinguished => true,
                ConsistencyMode::OnlyLast
                | ConsistencyMode::OnlyDistinguished
                | ConsistencyMode::LastAndDistinguished
                | ConsistencyMode::NeitherLastNorDistinguished => false,
            }
        }
    }

    // This could be tested in [`libsignal_keytrans::verify`],
    // but we have all the sample data here conveniently.
    #[test_case(ConsistencyMode::UnexpectedLast)]
    #[test_case(ConsistencyMode::UnexpectedDistinguished)]
    #[test_case(ConsistencyMode::EmptyLast)]
    #[test_case(ConsistencyMode::EmptyDistinguished)]
    #[test_case(ConsistencyMode::OnlyLast)]
    #[test_case(ConsistencyMode::OnlyDistinguished)]
    #[test_case(ConsistencyMode::LastAndDistinguished)]
    #[test_case(ConsistencyMode::NeitherLastNorDistinguished)]
    fn consistency_proofs_verification(mode: ConsistencyMode) {
        let valid_at = SystemTime::UNIX_EPOCH + CHAT_SEARCH_RESPONSE_VALID_AT;

        let aci = Aci::from(test_account::ACI);
        let e164 = test_account::PHONE_NUMBER;
        let username_hash = UsernameHash(Cow::Borrowed(test_account::USERNAME_HASH));

        let kt_impl = make_key_transparency();
        let mut search_response = test_search_response();

        let account_data =
            AccountData::try_from(test_account_data()).expect("valid stored account data");
        let mut account_data = Some(account_data);
        let distinguished_tree = test_distinguished_tree();
        let mut distinguished_tree = Some(&distinguished_tree);

        match mode {
            ConsistencyMode::UnexpectedLast => {
                account_data = None;
            }
            ConsistencyMode::UnexpectedDistinguished => {
                distinguished_tree = None;
            }
            ConsistencyMode::EmptyLast => {
                search_response.full_tree_head.last = vec![];
            }
            ConsistencyMode::EmptyDistinguished => {
                search_response.full_tree_head.distinguished = vec![];
            }
            ConsistencyMode::OnlyLast => {
                distinguished_tree = None;
                search_response.full_tree_head.distinguished = vec![];
            }
            ConsistencyMode::OnlyDistinguished => {
                account_data = None;
                search_response.full_tree_head.last = vec![];
            }
            ConsistencyMode::LastAndDistinguished => {
                // no modifications needed
            }
            ConsistencyMode::NeitherLastNorDistinguished => {
                account_data = None;
                distinguished_tree = None;
                search_response.full_tree_head.distinguished = vec![];
                search_response.full_tree_head.last = vec![];
            }
        };

        let result = verify_chat_search_response(
            &kt_impl,
            &aci,
            Some(e164),
            Some(username_hash),
            account_data,
            search_response,
            distinguished_tree,
            valid_at,
        );

        if mode.is_failure() {
            assert_matches!(result, Err(Error::VerificationFailed(_)));
        } else {
            assert_matches!(result, Ok(_))
        }
    }
}
