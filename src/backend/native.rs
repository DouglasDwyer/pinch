use crate::*;

use datachannel::DataChannelInit;
use futures::*;
use futures::channel::*;
use std::sync::Arc;
use std::cell::*;
use std::collections::*;
use std::task::*;

pub struct RtcPeerConnector {
    handle: Arc<RefCell<Box<datachannel::RtcPeerConnection<RtcPeerConnectionEventHandlers>>>>,
    message_handler: Box<dyn RtcMessageHandler>,
    negotiation_send: mpsc::UnboundedSender<RtcNegotiationNotification>,
    negotiation_recv: RefCell<mpsc::UnboundedReceiver<RtcNegotiationNotification>>
}

impl RtcPeerConnector {
    pub async fn connect(mut builder: RtcPeerConnection, message_handler: Box<dyn RtcMessageHandler>) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        let (negotiation_send, negotiation_recv) = mpsc::unbounded();
        let handle = Arc::new(RefCell::new(datachannel::RtcPeerConnection::new(&builder.configuration().into(), RtcPeerConnectionEventHandlers::new(negotiation_send.clone())).map_err(|x| RtcPeerConnectionError::Creation(format!("{:?}", x)))?));
        let negotiation_recv = RefCell::new(negotiation_recv);

        Self { handle, message_handler, negotiation_send, negotiation_recv }.create_channels(&mut builder).await
    }

    async fn create_channels(&mut self, builder: &mut RtcPeerConnection) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        self.assign_channel_ids(builder)?;
        self.accept_connection(builder).await
    }

    async fn accept_connection(&mut self, builder: &RtcPeerConnection) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        let mut chans = self.initialize_channels(builder).await?;
        let mut channels = Vec::with_capacity(builder.channels.len());
        
        while channels.len() < builder.channels.len() {
            let (p, rec) = self.negotiate().await?;
            let mut r: Vec<Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>> = Self::drain_where(&mut chans, |f| f.stream() as u16 == p);
            if r.len() != 1 {
                return Err(RtcPeerConnectionError::DataChannelMismatch("Data channel IDs did not match".to_string()));
            }
            else {
                channels.push(RtcDataChannel::new(r.pop().unwrap(), rec, self.handle.clone()));
            }
        }

        let mut ret = vec!();

        for bc in &builder.channels {
            let mut r: Vec<RtcDataChannel> = Self::drain_where(&mut channels, |f| f.id() == bc.configuration.id.unwrap());
            if r.len() != 1 {
                return Err(RtcPeerConnectionError::DataChannelMismatch("Data channel IDs did not match".to_string()));
            }
            else {
                ret.push(r.pop().unwrap());
            }
        }
        
        Ok(ret)
    }

    fn drain_where<T, Pred : Fn(&T) -> bool>(source: &mut Vec<T>, pred: Pred) -> Vec<T> {
        let mut src_new = vec!();
        let mut prd_new = vec!();

        for a in source.drain(..) {
            if pred(&a) {
                prd_new.push(a);
            }
            else {
                src_new.push(a);
            }
        }

        *source = src_new;

        prd_new
    }


    fn assign_channel_ids(&self, builder: &mut RtcPeerConnection) -> Result<(), RtcPeerConnectionError> {
        let mut chan_map = HashSet::new();

        for ch in &mut builder.channels {
            if let Some(id) = ch.configuration.id {
                if chan_map.contains(&id) {
                    return Err(RtcPeerConnectionError::Creation("Multiple data channels requested with the same ID".to_string()));
                }
                else {
                    chan_map.insert(id);
                }
            }
        }

        for ch in &mut builder.channels {
            if ch.configuration.id.is_none() {
                let mut id = chan_map.len() as u16;
                while chan_map.contains(&id) { id += 1; }
                ch.configuration.id = Some(id);
                ch.configuration.negotiated = Some(true);
                chan_map.insert(id);
            }
        }

        Ok(())
    }

    async fn initialize_channels(&mut self, builder: &RtcPeerConnection) -> Result<Vec<Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>>, RtcPeerConnectionError> {
        let mut channel_setup = vec!();

        for ch in &builder.channels {
            let dc = self.handle.borrow_mut().create_data_channel_ex(ch.label(), RtcDataChannelEventHandlers::new(ch.configuration().id.unwrap(), self.negotiation_send.clone()), &ch.configuration().into())
                .map_err(|x| RtcPeerConnectionError::Creation(format!("{:?}", x)))?;
            channel_setup.push(dc);
        }

        Ok(channel_setup)
    }

    async fn negotiate(&mut self) -> Result<(u16, mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>), RtcPeerConnectionError> {
        let mut candidate_buffer = vec!();
        loop {
            let ngt = match self.negotiation_recv.borrow_mut().try_next() {
                Ok(Some(x)) => Some(x),
                Err(_) => None,
                _ => unimplemented!()
            };

            if let Some(n) = ngt {
                match n {
                    RtcNegotiationNotification::SendMessage(m) => self.message_handler.send(m).await?,
                    RtcNegotiationNotification::DataChannelOpened(m) => return Ok(m),
                    RtcNegotiationNotification::Failed(m) => return Err(m)
                }
            }
            else if let Some(m) = self.message_handler.receive().await? {
                self.receive_negotiation_message(m, &mut candidate_buffer).await?;
            }

            PollFuture::once().await;
        }
    }

    async fn receive_negotiation_message(&mut self, message: RtcNegotiationMessage, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        match message {
            RtcNegotiationMessage::RemoteCandidate(c) => {
                if self.has_remote_description() {
                    self.add_remote_candidate(c).await
                }
                else {
                    candidate_buffer.push(c);
                    Ok(())
                }
            },
            RtcNegotiationMessage::RemoteSessionDescription(c) => {
                if !self.has_remote_description() {
                    let ci = datachannel::SessionDescription {
                        sdp: webrtc_sdp::parse_sdp(c.sdp.as_str(), false).map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?,
                        sdp_type: match c.sdp_type.as_str() {
                            "answer" => datachannel::SdpType::Answer,
                            "offer" => datachannel::SdpType::Offer,
                            "pranswer" => datachannel::SdpType::Pranswer,
                            "rollback" => datachannel::SdpType::Rollback,
                            _ => datachannel::SdpType::Offer
                        }
                    };

                    self.handle.borrow_mut().set_remote_description(&ci).map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

                    for cand in candidate_buffer.drain(..) {
                        self.add_remote_candidate(cand).await?;
                    }

                    Ok(())
                }
                else {
                    Ok(())
                }
            },
        }
    }

    fn has_remote_description(&self) -> bool {
        self.handle.borrow_mut().remote_description().is_some()
    }

    async fn add_remote_candidate(&mut self, c: RtcIceCandidate) -> Result<(), RtcPeerConnectionError> {
        self.handle.borrow_mut().add_remote_candidate(&c.into()).map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))
    }
}

enum RtcNegotiationNotification {
    SendMessage(RtcNegotiationMessage),
    DataChannelOpened((u16, mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>)),
    Failed(RtcPeerConnectionError)
}

struct RtcPeerConnectionEventHandlers {
    negotiation_send: mpsc::UnboundedSender<RtcNegotiationNotification>
}

impl RtcPeerConnectionEventHandlers {
    pub fn new(negotiation_send: mpsc::UnboundedSender<RtcNegotiationNotification>) -> Self {
        Self { negotiation_send }
    }
}

impl datachannel::PeerConnectionHandler for RtcPeerConnectionEventHandlers {
    type DCH = RtcDataChannelEventHandlers;

    fn data_channel_handler(&mut self) -> Self::DCH {
        unimplemented!()
    }

    fn on_candidate(&mut self, cand: datachannel::IceCandidate) {
        let _ = self.negotiation_send.unbounded_send(RtcNegotiationNotification::SendMessage(cand.into()));
    }

    fn on_description(&mut self, sess_desc: datachannel::SessionDescription) {
        let _ = self.negotiation_send.unbounded_send(RtcNegotiationNotification::SendMessage(sess_desc.into()));
    }

    fn on_connection_state_change(&mut self, state: datachannel::ConnectionState) {
        if state == datachannel::ConnectionState::Failed {
            let _ = self.negotiation_send.unbounded_send(RtcNegotiationNotification::Failed(RtcPeerConnectionError::IceNegotiationFailure("ICE protocol could not find a valid candidate pair.".to_string())));
        }
    }
}

pub struct RtcDataChannel {
    handle: RefCell<Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>>,
    label: String,
    message_recv: RefCell<mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>>,
    state: Cell<RtcDataChannelReadyState>,
    _peer_connection: Arc<RefCell<Box<datachannel::RtcPeerConnection<RtcPeerConnectionEventHandlers>>>>
}

impl RtcDataChannel {
    fn new(handle: Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>, message_recv: mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>, _peer_connection: Arc<RefCell<Box<datachannel::RtcPeerConnection<RtcPeerConnectionEventHandlers>>>>) -> Self {
        let label = handle.label();
        let handle = RefCell::new(handle);
        let message_recv = RefCell::new(message_recv);
        let state = Cell::new(RtcDataChannelReadyState::Open);

        Self { handle, label, message_recv, state, _peer_connection }
    }

    pub fn id(&self) -> u16 {
        self.handle.borrow_mut().stream() as u16
    }

    pub fn label(&self) -> &str {
        self.label.as_str()
    }

    pub fn ready_state(&self) -> RtcDataChannelReadyState {
        self.state.get()
    }

    pub fn send(&self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        let rq = self.handle.borrow_mut().send(message).map_err(|x| RtcDataChannelError::Send(format!("{:?}", x)));

        if rq.is_err() {
            self.state.set(RtcDataChannelReadyState::Closed);
        }

        rq
    }

    pub fn receive(&self, buffer: &mut impl std::io::Write) -> Result<Option<usize>, RtcDataChannelError> {
        let rq = match self.message_recv.borrow_mut().try_next() {
            Ok(Some(x)) => { let b = x?; buffer.write(&b[..]).map_err(|x| RtcDataChannelError::WriteError(x))?; Ok(Some(b.len())) },
            Err(_) => Ok(None),
            _ => unimplemented!()
        };

        if rq.is_err() {
            self.state.set(RtcDataChannelReadyState::Closed);
        }

        rq
    }

    pub async fn receive_async(&self, buffer: &mut impl std::io::Write) -> Result<usize, RtcDataChannelError> {
        loop {
            if let Some(x) = self.receive(buffer)? {
                return Ok(x);
            }

            PollFuture::once().await;
        }
    }

    pub fn receive_vec(&self) -> Result<Option<Vec<u8>>, RtcDataChannelError> {
        let rq = match self.message_recv.borrow_mut().try_next() {
            Ok(Some(x)) => x.map(|q| Some(q)),
            Err(_) => Ok(None),
            _ => unimplemented!()
        };

        if rq.is_err() {
            self.state.set(RtcDataChannelReadyState::Closed);
        }

        rq
    }

    pub async fn receive_vec_async(&self) -> Result<Vec<u8>, RtcDataChannelError> {
        loop {
            if let Some(x) = self.receive_vec()? {
                return Ok(x);
            }

            PollFuture::once().await;
        }
    }
}

struct RtcDataChannelEventHandlers {
    id: u16,
    negotiation_send: Option<mpsc::UnboundedSender<RtcNegotiationNotification>>,
    message_recv: Option<mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>>,
    message_send: mpsc::UnboundedSender<Result<Vec<u8>, RtcDataChannelError>>
}

impl RtcDataChannelEventHandlers {
    pub fn new(id: u16, negotiation_send: mpsc::UnboundedSender<RtcNegotiationNotification>) -> Self {
        let negotiation_send = Some(negotiation_send);
        let (message_send, message_recv) = mpsc::unbounded();
        let message_recv = Some(message_recv);

        Self { id, negotiation_send, message_recv, message_send }
    }
}

impl datachannel::DataChannelHandler for RtcDataChannelEventHandlers {
    fn on_open(&mut self) {
        if let Some(ns) = &self.negotiation_send {
            let _ = ns.unbounded_send(RtcNegotiationNotification::DataChannelOpened((self.id, std::mem::replace(&mut self.message_recv, None).unwrap())));
            drop(ns);
            self.negotiation_send = None;
        }
    }

    fn on_closed(&mut self) {
        let _ = self.message_send.unbounded_send(Err(RtcDataChannelError::Receive("The channel was closed.".to_string())));
    }

    fn on_error(&mut self, err: &str) {
        let _ = self.message_send.unbounded_send(Err(RtcDataChannelError::Receive(format!("The channel encountered an error: {}", err))));
    }

    fn on_message(&mut self, msg: &[u8]) {
        let _ = self.message_send.unbounded_send(Ok(msg.to_vec()));
    }

    fn on_buffered_amount_low(&mut self) {}

    fn on_available(&mut self) {}
}


pub struct PollFuture {
    count: u32
}

impl PollFuture {
    pub fn new(count: u32) -> PollFuture {
        PollFuture { count }
    }

    pub fn once() -> PollFuture {
        PollFuture::new(1)
    }
}

impl Future for PollFuture {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if self.count > 0 {
            self.get_mut().count -= 1;
            return Poll::Pending;
        }

        Poll::Ready(())
    }
}

impl From<datachannel::SessionDescription> for RtcNegotiationMessage {
    fn from(x: datachannel::SessionDescription) -> Self {
        Self::RemoteSessionDescription(RtcSessionDescription {
            sdp: x.sdp.to_string(),
            sdp_type: match x.sdp_type {
                datachannel::SdpType::Answer => "answer",
                datachannel::SdpType::Offer => "offer",
                datachannel::SdpType::Pranswer => "pranswer",
                datachannel::SdpType::Rollback => "rollback",
            }.to_string()
        })
    }
}

impl From<datachannel::IceCandidate> for RtcNegotiationMessage {
    fn from(x: datachannel::IceCandidate) -> Self {
        Self::RemoteCandidate(RtcIceCandidate {
            candidate: x.candidate,
            sdp_mid: x.mid
        })
    }
}

impl From<RtcIceCandidate> for datachannel::IceCandidate {
    fn from(x: RtcIceCandidate) -> Self {
        Self {
            candidate: x.candidate,
            mid: x.sdp_mid
        }
    }
}

fn format_ice_url(url: &String, username: &Option<String>, credential: &Option<String>) -> String {
    let mut en = url.clone();

    if let Some(x) = username {
        let qr = if let Some(y) = credential { ":".to_string() + y } else { "".to_string() };
        en = x.to_owned() + &qr + &"@".to_string() + &en;
    }

    "turn:".to_owned() + &en
}

impl From<&RtcConfiguration> for datachannel::RtcConfig {
    fn from(x: &RtcConfiguration) -> Self {
        let mut is = vec!();

        for s in &x.ice_servers {
            for u in &s.urls {
                if u.starts_with("turn:") {
                    is.push(format_ice_url(&u.replacen("turn:", "", 1), &s.username, &s.credential));
                }
                else {
                    is.push(u.clone());
                }
            }
        }

        let mut ret = Self::new(&is[..]);
        ret.ice_transport_policy = x.ice_transport_policy.into();

        ret
    }
}

impl From<RtcIceTransportPolicy> for datachannel::TransportPolicy {
    fn from(x: RtcIceTransportPolicy) -> Self {
        match x {
            RtcIceTransportPolicy::All => Self::All,
            RtcIceTransportPolicy::Relay => Self::Relay
        }
    }
}

impl From<&RtcDataChannelConfiguration> for DataChannelInit {
    fn from(x: &RtcDataChannelConfiguration) -> Self {
        let mut ret = Self::default()
            .protocol(x.protocol.as_ref().map(|x| x.clone()).unwrap_or("".to_string()).as_str())
            .reliability(datachannel::Reliability { unordered: !x.ordered.unwrap_or(true), unreliable: !x.ordered.unwrap_or(true), max_packet_life_time: x.max_packet_life_time.unwrap_or(0), max_retransmits: x.max_retransmits.unwrap_or(0) });

        if x.negotiated.unwrap_or(false) {
            ret = ret.negotiated().manual_stream();
        }

        if let Some(q) = x.id {
            ret = ret.stream(q);
        }

        ret
    }
}