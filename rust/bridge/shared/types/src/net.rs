//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::panic::RefUnwindSafe;
use std::sync::Arc;

use aes_gcm_siv::aead::rand_core::CryptoRngCore;
use async_trait::async_trait;
use futures_util::future::join3;
use libsignal_net::auth::Auth;
use libsignal_net::connect_state::{ConnectState, SUGGESTED_CONNECT_CONFIG};
use libsignal_net::enclave::{
    Cdsi, EnclaveEndpoint, EnclaveEndpointConnection, EnclaveKind, Nitro, PpssSetup, Sgx, Tpm2Snp,
};
use libsignal_net::env::{add_user_agent_header, Env, Svr3Env, UserAgent};
use libsignal_net::infra::connection_manager::MultiRouteConnectionManager;
use libsignal_net::infra::dns::DnsResolver;
use libsignal_net::infra::route::ConnectionProxyConfig;
use libsignal_net::infra::tcp_ssl::{InvalidProxyConfig, TcpSslConnector};
use libsignal_net::infra::timeouts::ONE_ROUTE_CONNECTION_TIMEOUT;
use libsignal_net::infra::utils::ObservableEvent;
use libsignal_net::infra::{EnableDomainFronting, EndpointConnection};
use libsignal_net::svr::SvrConnection;
use libsignal_net::svr3::traits::*;
use libsignal_net::svr3::{Error, OpaqueMaskedShareSet};
use libsignal_svr3::EvaluationResult;

use crate::*;

pub mod cdsi;
pub mod chat;
pub mod tokio;

pub use tokio::TokioAsyncContext;

#[derive(num_enum::TryFromPrimitive)]
#[repr(u8)]
#[derive(Clone, Copy, strum::Display)]
pub enum Environment {
    Staging = 0,
    Prod = 1,
}

impl Environment {
    pub fn env<'a>(self) -> Env<'a, Svr3Env<'a>> {
        match self {
            Self::Staging => libsignal_net::env::STAGING,
            Self::Prod => libsignal_net::env::PROD,
        }
    }
}

type Svr3EndpointConnections = (
    EnclaveEndpointConnection<Sgx, MultiRouteConnectionManager>,
    EnclaveEndpointConnection<Nitro, MultiRouteConnectionManager>,
    EnclaveEndpointConnection<Tpm2Snp, MultiRouteConnectionManager>,
);

struct EndpointConnections {
    chat: EndpointConnection<MultiRouteConnectionManager>,
    cdsi: EnclaveEndpointConnection<Cdsi, MultiRouteConnectionManager>,
    svr3: Svr3EndpointConnections,
    enable_fronting: EnableDomainFronting,
}

impl EndpointConnections {
    fn new(
        env: &Env<'static, Svr3Env<'static>>,
        user_agent: &UserAgent,
        use_fallbacks: bool,
        network_change_event: &ObservableEvent,
    ) -> Self {
        log::info!(
            "Creating endpoint connections (fallbacks {}) for {} and others",
            if use_fallbacks { "enabled" } else { "disabled" },
            // Note: this is *not* using log_safe_domain, because it is always the direct route.
            // Either it's chat.signal.org, chat.staging.signal.org, or something that indicates
            // testing. (Or the person running this isn't Signal.)
            env.chat_domain_config.connect.hostname
        );
        let chat = libsignal_net::chat::endpoint_connection(
            &env.chat_domain_config.connect,
            user_agent,
            use_fallbacks,
            network_change_event,
        );
        let cdsi =
            Self::endpoint_connection(&env.cdsi, user_agent, use_fallbacks, network_change_event);
        let svr3 = (
            Self::endpoint_connection(
                env.svr3.sgx(),
                user_agent,
                use_fallbacks,
                network_change_event,
            ),
            Self::endpoint_connection(
                env.svr3.nitro(),
                user_agent,
                use_fallbacks,
                network_change_event,
            ),
            Self::endpoint_connection(
                env.svr3.tpm2snp(),
                user_agent,
                use_fallbacks,
                network_change_event,
            ),
        );
        Self {
            chat,
            cdsi,
            svr3,
            enable_fronting: EnableDomainFronting(use_fallbacks),
        }
    }

    fn endpoint_connection<E: EnclaveKind>(
        endpoint: &EnclaveEndpoint<'static, E>,
        user_agent: &UserAgent,
        include_fallback: bool,
        network_change_event: &ObservableEvent,
    ) -> EnclaveEndpointConnection<E, MultiRouteConnectionManager> {
        let params = if include_fallback {
            endpoint
                .domain_config
                .connect
                .connection_params_with_fallback()
        } else {
            vec![endpoint.domain_config.connect.direct_connection_params()]
        };
        let params = add_user_agent_header(params, user_agent);
        EnclaveEndpointConnection::new_multi(
            endpoint,
            params,
            ONE_ROUTE_CONNECTION_TIMEOUT,
            network_change_event,
        )
    }
}

pub struct ConnectionManager {
    env: Env<'static, Svr3Env<'static>>,
    user_agent: UserAgent,
    dns_resolver: DnsResolver,
    connect: ::tokio::sync::RwLock<ConnectState>,
    // We could split this up to a separate mutex on each kind of connection,
    // but we don't hold it for very long anyway (just enough to clone the Arc).
    endpoints: std::sync::Mutex<Arc<EndpointConnections>>,
    transport_connector: std::sync::Mutex<TcpSslConnector>,
    network_change_event: ObservableEvent,
}

impl RefUnwindSafe for ConnectionManager {}

impl ConnectionManager {
    pub fn new(environment: Environment, user_agent: &str) -> Self {
        log::info!("Initializing connection manager for {}...", &environment);
        Self::new_from_static_environment(environment.env(), user_agent)
    }

    pub fn new_from_static_environment(
        env: Env<'static, Svr3Env<'static>>,
        user_agent: &str,
    ) -> Self {
        let network_change_event = ObservableEvent::new();
        let user_agent = UserAgent::with_libsignal_version(user_agent);

        let dns_resolver =
            DnsResolver::new_with_static_fallback(env.static_fallback(), &network_change_event);
        let transport_connector =
            std::sync::Mutex::new(TcpSslConnector::new_direct(dns_resolver.clone()));
        let endpoints = std::sync::Mutex::new(
            EndpointConnections::new(&env, &user_agent, false, &network_change_event).into(),
        );
        Self {
            env,
            endpoints,
            user_agent,
            connect: ConnectState::new(SUGGESTED_CONNECT_CONFIG),
            dns_resolver,
            transport_connector,
            network_change_event,
        }
    }

    pub fn set_proxy(&self, proxy: ConnectionProxyConfig) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.set_proxy(proxy);
    }

    pub fn set_invalid_proxy(&self) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.set_invalid();
    }

    pub fn clear_proxy(&self) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.clear_proxy();
    }

    pub fn is_using_proxy(&self) -> Result<bool, InvalidProxyConfig> {
        let guard = self.transport_connector.lock().expect("not poisoned");
        guard.proxy().map(|proxy| proxy.is_some())
    }

    pub fn set_ipv6_enabled(&self, ipv6_enabled: bool) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.set_ipv6_enabled(ipv6_enabled);
        self.connect.blocking_write().route_resolver.allow_ipv6 = ipv6_enabled;
    }

    /// Resets the endpoint connections to include or exclude censorship circumvention routes.
    ///
    /// This is not itself a network change event; existing working connections are expected to
    /// continue to work, and existing failing connections will continue to fail.
    pub fn set_censorship_circumvention_enabled(&self, enabled: bool) {
        let new_endpoints = EndpointConnections::new(
            &self.env,
            &self.user_agent,
            enabled,
            &self.network_change_event,
        );
        *self.endpoints.lock().expect("not poisoned") = Arc::new(new_endpoints);
    }

    pub fn on_network_change(&self) {
        log::info!("ConnectionManager: on_network_change");
        self.network_change_event.fire()
    }
}

bridge_as_handle!(ConnectionManager);
bridge_as_handle!(ConnectionProxyConfig);

pub enum PreviousVersion {}
pub enum CurrentVersion {}

pub struct Svr3Client<'a, Kind> {
    connection_manager: &'a ConnectionManager,
    auth: Auth,
    kind: PhantomData<Kind>,
}

impl<'a, Kind> Svr3Client<'a, Kind> {
    fn new(connection_manager: &'a ConnectionManager, auth: Auth) -> Self {
        Self {
            connection_manager,
            auth,
            kind: PhantomData,
        }
    }
}

pub struct Svr3Clients<'a> {
    pub previous: Svr3Client<'a, PreviousVersion>,
    pub current: Svr3Client<'a, CurrentVersion>,
}

impl<'a> Svr3Clients<'a> {
    pub fn new(
        connection_manager: &'a ConnectionManager,
        username: String,
        password: String,
    ) -> Self {
        let auth = Auth { username, password };
        Self {
            previous: Svr3Client::new(connection_manager, auth.clone()),
            current: Svr3Client::new(connection_manager, auth),
        }
    }
}

#[async_trait]
impl Svr3Connect for Svr3Client<'_, CurrentVersion> {
    type Env = Svr3Env<'static>;

    async fn connect(&self) -> <Self::Env as PpssSetup>::ConnectionResults {
        let ConnectionManager {
            endpoints,
            transport_connector,
            ..
        } = &self.connection_manager;
        let transport_connector = transport_connector.lock().expect("not poisoned").clone();
        let endpoints = endpoints.lock().expect("not poisoned").clone();
        let (sgx, nitro, tpm2snp) = &endpoints.svr3;
        let (sgx, nitro, tpm2snp) = join3(
            SvrConnection::connect(self.auth.clone(), sgx, transport_connector.clone()),
            SvrConnection::connect(self.auth.clone(), nitro, transport_connector.clone()),
            SvrConnection::connect(self.auth.clone(), tpm2snp, transport_connector),
        )
        .await;
        (sgx, nitro, tpm2snp)
    }
}

#[async_trait]
impl Backup for Svr3Client<'_, PreviousVersion> {
    async fn backup(
        &self,
        _password: &str,
        _secret: [u8; 32],
        _max_tries: NonZeroU32,
        _rng: &mut (impl CryptoRngCore + Send),
    ) -> Result<OpaqueMaskedShareSet, Error> {
        empty_env::backup().await
    }
}

#[async_trait]
impl Restore for Svr3Client<'_, PreviousVersion> {
    async fn restore(
        &self,
        _password: &str,
        _share_set: OpaqueMaskedShareSet,
        _rng: &mut (impl CryptoRngCore + Send),
    ) -> Result<EvaluationResult, Error> {
        empty_env::restore().await
    }
}

#[async_trait]
impl Remove for Svr3Client<'_, PreviousVersion> {
    async fn remove(&self) -> Result<(), Error> {
        empty_env::remove().await
    }
}

#[async_trait]
impl Query for Svr3Client<'_, PreviousVersion> {
    async fn query(&self) -> Result<u32, Error> {
        empty_env::query().await
    }
}

#[async_trait]
impl Rotate for Svr3Client<'_, PreviousVersion> {
    async fn rotate(
        &self,
        _share_set: OpaqueMaskedShareSet,
        _rng: &mut (impl CryptoRngCore + Send),
    ) -> Result<(), Error> {
        empty_env::rotate().await
    }
}

// These functions define the behavior of the empty `PreviousVersion`
// when there is no migration going on.
// When there _is_ migration both current and previous clients should instead
// implement `Svr3Connect` and use the blanket implementations of the traits.
mod empty_env {
    use super::*;

    pub async fn backup() -> Result<OpaqueMaskedShareSet, Error> {
        // Ideally it would be a panic, as this is certainly a programmer error
        // that needs to be fixed. However, panics are propagated to the clients
        // and become runtime exceptions that can be caught. This way we will
        // don't change the set of expected errors on the client side, and get a
        // descriptive message in the logs.
        Err(Error::AttestationError(
            attest::enclave::Error::AttestationDataError {
                reason: "This SVR3 environment does not exist".to_string(),
            },
        ))
    }

    pub async fn restore() -> Result<EvaluationResult, Error> {
        // This is the only error value that will make `restore_with_fallback`
        // function effectively ignore this SVR3 setup, thus making it possible
        // to use `restore_with_fallback` for all restores unconditionally.
        Err(Error::DataMissing)
    }

    pub async fn remove() -> Result<(), Error> {
        Ok(())
    }

    pub async fn query() -> Result<u32, Error> {
        Err(Error::DataMissing)
    }

    pub async fn rotate() -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use ::tokio; // otherwise ambiguous with the tokio submodule
    use assert_matches::assert_matches;
    use libsignal_net::chat::ChatServiceError;
    use test_case::test_case;

    use super::*;
    use crate::net::chat::UnauthenticatedChatConnection;

    #[test_case(Environment::Staging; "staging")]
    #[test_case(Environment::Prod; "prod")]
    fn can_create_connection_manager(env: Environment) {
        let _ = ConnectionManager::new(env, "test-user-agent");
    }

    // Normally we would write this test in the app languages, but it depends on timeouts.
    // Using a paused tokio runtime auto-advances time when there's no other work to be done.
    #[tokio::test(start_paused = true)]
    async fn cannot_connect_through_invalid_proxy() {
        let cm = ConnectionManager::new(Environment::Staging, "test-user-agent");
        cm.set_invalid_proxy();
        let err = UnauthenticatedChatConnection::connect(&cm)
            .await
            .map(|_| ())
            .expect_err("should fail to connect");
        assert_matches!(err, ChatServiceError::ServiceUnavailable);
    }
}
