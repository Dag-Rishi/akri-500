/// Akri's Discovery API code, which is auto-generated by `build.rs` from `proto/discovery.proto`
pub mod v0;

/// Definition of the DiscoverStream type expected for supported embedded Akri DiscoveryHandlers
pub type DiscoverStream = tokio::sync::mpsc::Receiver<Result<v0::DiscoverResponse, tonic::Status>>;

pub mod discovery_handler {
    use super::super::registration_client::{
        register_discovery_handler, register_discovery_handler_again,
    };
    use super::{
        server::run_discovery_server,
        v0::{
            discovery_handler_server::DiscoveryHandler,
            register_discovery_handler_request::EndpointType, RegisterDiscoveryHandlerRequest,
        },
    };
    use log::trace;
    use tokio::sync::mpsc;

    const DISCOVERY_PORT: i16 = 10000;

    /// Capacity of channel over which a message is sent by `DiscoveryHandler::discover` that its `DiscoveryHandler`
    /// should re-register due to the Agent dropping its end of the current connection.
    pub const REGISTER_AGAIN_CHANNEL_CAPACITY: usize = 1;

    /// Capacity of channel over which discovery handlers send updates to clients about currently discovered devices. It
    /// is assumed that clients are always listening for updates; however, the size is increased to account for any delays
    /// in receiving.
    pub const DISCOVERED_DEVICES_CHANNEL_CAPACITY: usize = 4;

    pub async fn run_discovery_handler(
        discovery_handler: impl DiscoveryHandler,
        register_receiver: mpsc::Receiver<()>,
        protocol_name: &str,
        shared: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let mut use_uds = true;
        let mut endpoint: String = match std::env::var("POD_IP") {
            Ok(pod_ip) => {
                trace!("run_discovery_handler - registering with Agent with IP endpoint");
                use_uds = false;
                format!("{}:{}", pod_ip, DISCOVERY_PORT)
            }
            Err(_) => {
                trace!("run_discovery_handler - registering with Agent with uds endpoint");
                format!(
                    "{}/{}.sock",
                    std::env::var(super::super::DISCOVERY_HANDLERS_DIRECTORY_LABEL).unwrap(),
                    protocol_name
                )
            }
        };
        let endpoint_clone = endpoint.clone();
        let discovery_handle = tokio::spawn(async move {
            run_discovery_server(discovery_handler, &endpoint_clone)
                .await
                .unwrap();
        });
        let endpoint_type = if !use_uds {
            endpoint.insert_str(0, "http://");
            EndpointType::Network
        } else {
            EndpointType::Uds
        };
        let register_request = RegisterDiscoveryHandlerRequest {
            name: protocol_name.to_string(),
            endpoint,
            endpoint_type: endpoint_type as i32,
            shared,
        };
        register_discovery_handler(&register_request).await?;
        let registration_handle = tokio::spawn(async move {
            register_discovery_handler_again(register_receiver, &register_request).await;
        });
        tokio::try_join!(discovery_handle, registration_handle)?;
        Ok(())
    }

    /// This obtains the expected type `T` from a discovery details String by running it through function `f` which will
    /// attempt to deserialize the String.
    pub fn deserialize_discovery_details<T>(discovery_details: &str) -> Result<T, anyhow::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        let discovery_handler_config: T = serde_yaml::from_str(discovery_details).map_err(|e| {
            anyhow::format_err!(
                "Configuration discovery details improperly configured with error {:?}",
                e
            )
        })?;
        Ok(discovery_handler_config)
    }
}

#[cfg(any(feature = "mock-discovery-handler", test))]
pub mod mock_discovery_handler {
    use super::v0::{
        discovery_handler_server::DiscoveryHandler, DiscoverRequest, DiscoverResponse,
    };
    use akri_shared::uds::unix_stream;
    use async_trait::async_trait;
    use tempfile::Builder;
    use tokio::sync::mpsc;

    /// Simple discovery handler for tests
    pub struct MockDiscoveryHandler {
        pub return_error: bool,
    }

    #[async_trait]
    impl DiscoveryHandler for MockDiscoveryHandler {
        type DiscoverStream = super::DiscoverStream;
        async fn discover(
            &self,
            _: tonic::Request<DiscoverRequest>,
        ) -> Result<tonic::Response<Self::DiscoverStream>, tonic::Status> {
            let (mut discovered_devices_sender, discovered_devices_receiver) =
                mpsc::channel(super::discovery_handler::DISCOVERED_DEVICES_CHANNEL_CAPACITY);
            tokio::spawn(async move {
                discovered_devices_sender
                    .send(Ok(DiscoverResponse {
                        devices: Vec::new(),
                    }))
                    .await
                    .unwrap();
            });
            // Conditionally return error if specified
            if self.return_error {
                Err(tonic::Status::invalid_argument(
                    "mock discovery handler error",
                ))
            } else {
                Ok(tonic::Response::new(discovered_devices_receiver))
            }
        }
    }

    pub fn get_mock_discovery_handler_dir_and_endpoint(socket_name: &str) -> (String, String) {
        let discovery_handler_temp_dir = Builder::new()
            .prefix("discovery-handlers")
            .tempdir()
            .unwrap();
        let discovery_handler_temp_dir_path = discovery_handler_temp_dir.path().join(socket_name);
        (
            discovery_handler_temp_dir
                .path()
                .to_str()
                .unwrap()
                .to_string(),
            discovery_handler_temp_dir_path
                .to_str()
                .unwrap()
                .to_string(),
        )
    }

    pub async fn run_mock_discovery_handler(
        discovery_handler_dir: &str,
        discovery_handler_endpoint: &str,
        return_error: bool,
    ) -> tokio::task::JoinHandle<()> {
        let discovery_handler = MockDiscoveryHandler { return_error };
        let discovery_handler_dir_string = discovery_handler_dir.to_string();
        let discovery_handler_endpoint_string = discovery_handler_endpoint.to_string();
        let handle = tokio::spawn(async move {
            super::server::internal_run_discovery_server(
                discovery_handler,
                &discovery_handler_endpoint_string,
                &discovery_handler_dir_string,
            )
            .await
            .unwrap();
        });

        // Try to connect in loop until first thread has served Discovery Handler
        unix_stream::try_connect(discovery_handler_endpoint)
            .await
            .unwrap();
        handle
    }
}

pub mod server {
    use super::v0::discovery_handler_server::{DiscoveryHandler, DiscoveryHandlerServer};
    use akri_shared::uds::unix_stream;
    use futures::stream::TryStreamExt;
    use log::info;
    use std::path::Path;
    use tokio::net::UnixListener;
    use tonic::transport::Server;

    pub async fn run_discovery_server(
        discovery_handler: impl DiscoveryHandler,
        discovery_endpoint: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        internal_run_discovery_server(
            discovery_handler,
            discovery_endpoint,
            &std::env::var(super::super::DISCOVERY_HANDLERS_DIRECTORY_LABEL).unwrap(),
        )
        .await
    }

    /// Creates a DiscoveryHandlerServer for the given Discovery Handler at the specified endpoint Verifies the endpoint
    /// by checking that it is in the discovery handler directory if it is UDS or that it is a valid IP address and
    /// port.
    pub async fn internal_run_discovery_server(
        discovery_handler: impl DiscoveryHandler,
        discovery_endpoint: &str,
        discovery_handler_directory: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        info!("internal_run_discovery_server - entered");

        if discovery_endpoint.starts_with(discovery_handler_directory) {
            tokio::fs::create_dir_all(Path::new(&discovery_endpoint[..]).parent().unwrap()).await?;
            // Delete socket if it already exists
            std::fs::remove_file(discovery_endpoint).unwrap_or(());
            let mut uds = UnixListener::bind(discovery_endpoint)?;
            Server::builder()
                .add_service(DiscoveryHandlerServer::new(discovery_handler))
                .serve_with_incoming(uds.incoming().map_ok(unix_stream::UnixStream))
                .await?;
            std::fs::remove_file(discovery_endpoint).unwrap_or(());
        } else {
            let addr = discovery_endpoint.parse()?;
            Server::builder()
                .add_service(DiscoveryHandlerServer::new(discovery_handler))
                .serve(addr)
                .await?;
        }
        info!("internal_run_discovery_server - finished");
        Ok(())
    }

    #[cfg(test)]
    pub mod tests {
        use super::super::{
            mock_discovery_handler::{
                get_mock_discovery_handler_dir_and_endpoint, run_mock_discovery_handler,
                MockDiscoveryHandler,
            },
            v0::{discovery_handler_client::DiscoveryHandlerClient, DiscoverRequest},
        };
        use super::*;
        use std::convert::TryFrom;
        use tempfile::Builder;
        use tokio::net::UnixStream;
        use tonic::{
            transport::{Endpoint, Uri},
            Request,
        };

        #[tokio::test]
        async fn test_run_discovery_server_uds() {
            let (discovery_handler_dir, discovery_handler_socket) =
                get_mock_discovery_handler_dir_and_endpoint("protocol.sock");
            let _handle: tokio::task::JoinHandle<()> = run_mock_discovery_handler(
                &discovery_handler_dir,
                &discovery_handler_socket,
                false,
            )
            .await;
            let channel = Endpoint::try_from("dummy://[::]:50051")
                .unwrap()
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    UnixStream::connect(discovery_handler_socket.clone())
                }))
                .await
                .unwrap();
            let mut discovery_handler_client = DiscoveryHandlerClient::new(channel);
            let mut stream = discovery_handler_client
                .discover(Request::new(DiscoverRequest {
                    discovery_details: String::new(),
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(stream.message().await.unwrap().unwrap().devices.is_empty());
        }

        // Test when improper socket path or IP address is given as an endpoint
        #[tokio::test]
        async fn test_run_discovery_server_error_invalid_ip_addr() {
            let discovery_handler = MockDiscoveryHandler {
                return_error: false,
            };
            let discovery_handler_temp_dir = Builder::new()
                .prefix("discovery-handlers")
                .tempdir()
                .unwrap();
            if let Err(e) = internal_run_discovery_server(
                discovery_handler,
                "random",
                discovery_handler_temp_dir.path().to_str().unwrap(),
            )
            .await
            {
                assert!((*e).to_string().contains("invalid IP address syntax"))
            } else {
                panic!("should be invalid IP address error")
            }
        }
    }
}
