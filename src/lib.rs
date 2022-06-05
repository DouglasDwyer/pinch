#![feature(unboxed_closures)]

mod backend;

use serde::*;
use thiserror::*;

#[derive(Clone)]
pub struct RtcPeerConnection {
    configuration: RtcConfiguration,
    attempt: RtcPeerConnectionAttempt,
    channels: Vec<RtcDataChannelBuilder>,
}

impl RtcPeerConnection {
    pub fn new(configuration: RtcConfiguration, attempt: RtcPeerConnectionAttempt, channels: Vec<RtcDataChannelBuilder>) -> Self {
        Self { configuration, attempt, channels }
    }

    pub fn configuration(&self) -> &RtcConfiguration {
        &self.configuration
    }

    pub fn attempt(&self) -> &RtcPeerConnectionAttempt {
        &self.attempt
    }

    pub async fn connect(self, message_handler: Box<dyn RtcMessageHandler>) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        Ok(backend::RtcPeerConnector::connect(self, message_handler).await?.into_iter().map(|x| RtcDataChannel { backend: x }).collect())
    }
}

#[derive(Clone)]
pub enum RtcPeerConnectionAttempt {
    Offer,
    Answer
}

#[derive(Clone)]
pub struct RtcDataChannelBuilder {
    label: String,
    configuration: RtcDataChannelConfiguration
}

impl RtcDataChannelBuilder {
    pub fn new(label: &str) -> Self {
        Self::new_with_options(label, RtcDataChannelConfiguration::default())
    }

    pub fn new_with_options(label: &str, configuration: RtcDataChannelConfiguration) -> Self {
        let label = label.to_string();
        Self { label, configuration }
    }

    pub fn label(&self) -> &str {
        self.label.as_str()
    }

    pub fn configuration(&self) -> &RtcDataChannelConfiguration {
        &self.configuration
    }
}

pub struct RtcDataChannel {
    backend: backend::RtcDataChannel
}

impl RtcDataChannel {
    pub fn id(&self) -> u16 {
        self.backend.id()
    }

    pub fn label(&self) -> &str {
        self.backend.label()
    }

    pub fn ready_state(&self) -> RtcDataChannelReadyState {
        self.backend.ready_state()
    }

    pub fn send(&self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        self.backend.send(message)
    }

    pub fn receive(&self, buffer: &mut impl std::io::Write) -> Result<Option<usize>, RtcDataChannelError> {
        self.backend.receive(buffer)
    }

    pub async fn receive_async(&self, buffer: &mut impl std::io::Write) -> Result<usize, RtcDataChannelError> {
        self.backend.receive_async(buffer).await
    }

    pub fn receive_vec(&self) -> Result<Option<Vec<u8>>, RtcDataChannelError> {
        self.backend.receive_vec()
    }

    pub async fn receive_vec_async(&self) -> Result<Vec<u8>, RtcDataChannelError> {
        self.backend.receive_vec_async().await
    }
}

#[derive(Error, Debug)]
pub enum RtcPeerConnectionError {
    #[error("An error occurred during connection creation: {0}")]
    Creation(String),
    #[error("An error occurred during channel negotiation: {0}")]
    Negotiation(String),
    #[error("The data channel configuration was not the same for both peers: {0}")]
    DataChannelMismatch(String),
    #[error("ICE negotiation failed: {0}")]
    IceNegotiationFailure(String)
}

#[derive(Error, Debug)]
pub enum RtcDataChannelError {
    #[error("An error occurred during message sending: {0}")]
    Send(String),
    #[error("An error occurred during message receiving: {0}")]
    Receive(String),
    #[error("An error occurred while writing to std::io::Write: {0}")]
    WriteError(std::io::Error)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RtcNegotiationMessage {
    RemoteCandidate(RtcIceCandidate),
    RemoteSessionDescription(RtcSessionDescription)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcIceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMid")]
    pub sdp_mid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcSessionDescription {
    pub sdp: String,
    #[serde(rename = "type")]
    pub sdp_type: String,
}

pub trait RtcMessageHandler {
    fn send(&self, message: RtcNegotiationMessage) -> Box<dyn std::future::Future<Output = Result<(), RtcPeerConnectionError>> + Unpin>;
    fn receive(&self) -> Box<dyn std::future::Future<Output = Result<Option<RtcNegotiationMessage>, RtcPeerConnectionError>> + Unpin>;
}

#[derive(Default, PartialEq, Clone, Debug)]
pub struct RtcConfiguration {
    pub ice_servers: Vec<RtcIceServer>,
    pub ice_transport_policy: RtcIceTransportPolicy
}

#[derive(Default, Clone, PartialEq, Debug)]
pub struct RtcIceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>
}

impl RtcIceServer {
    pub fn new(url: &str) -> Self {
        Self {
            urls: vec!(url.to_string()),
            ..Default::default()
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
#[repr(u32)]
pub enum RtcIceTransportPolicy {
    All,
    Relay,
}

impl Default for RtcIceTransportPolicy {
    fn default() -> Self {
        Self::All
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum RtcSignalingState {
    Stable,
    HaveLocalOffer,
    HaveRemoteOffer,
    HaveLocalPranswer,
    HaveRemotePranswer,
    Closed
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum RtcPeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

#[derive(Copy, Clone, PartialEq)]
pub enum RtcIceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum SdpType {
    Answer,
    Offer,
    Pranswer,
    Rollback,
}

#[derive(Clone, Default)]
pub struct RtcDataChannelConfiguration {
    pub ordered: Option<bool>,
    pub max_packet_life_time: Option<u16>,
    pub max_retransmits: Option<u16>,
    pub protocol: Option<String>,
    pub negotiated: Option<bool>,
    pub id: Option<u16>,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum RtcDataChannelReadyState {
    Connecting,
    Open,
    Closing,
    Closed
}