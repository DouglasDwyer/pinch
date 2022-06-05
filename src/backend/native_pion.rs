use crate::*;

use crate::*;

use futures::*;
use futures::channel::*;
use futures::channel::mpsc::UnboundedSender;
use log::*;
use std::sync::Arc;
use std::borrow::BorrowMut;
use std::cell::*;
use std::collections::*;
use std::rc::*;
use std::task::*;

pub struct RtcPeerConnector {
    handle: webrtc::peer_connection::RTCPeerConnection,
    message_handler: Box<dyn RtcMessageHandler>,
    negotiation_recv: RefCell<mpsc::UnboundedReceiver<RtcNegotiationNotification>>,
    negotiation_send: mpsc::UnboundedSender<RtcNegotiationNotification>
}

impl RtcPeerConnector {
    pub async fn connect(mut builder: RtcPeerConnection, message_handler: Box<dyn RtcMessageHandler>) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        let handle = webrtc::api::APIBuilder::default().build().new_peer_connection(builder.configuration().into()).await.map_err(|x| RtcPeerConnectionError::Creation(format!("{:?}", x)))?;

        let (negotiation_send, negotiation_recv) = mpsc::unbounded();
        let negotiation_recv = RefCell::new(negotiation_recv);
        Self::add_event_handlers(&handle, negotiation_send.clone()).await?;

        Self { handle, message_handler, negotiation_recv, negotiation_send }.create_channels(&mut builder).await
    }

    async fn add_event_handlers(handle: &webrtc::peer_connection::RTCPeerConnection, negotiation_send: UnboundedSender<RtcNegotiationNotification>) -> Result<(), RtcPeerConnectionError> {
        let ns = negotiation_send.clone();
        handle.on_ice_candidate(Box::new(move |x| {
            let ns = ns.clone();
            async move {
                if let Some(candidate) = x {
                    let cin = candidate.to_json().await.unwrap();
                    let _ = ns.unbounded_send(RtcNegotiationNotification::SendMessage(
                        RtcNegotiationMessage::RemoteCandidate(RtcIceCandidate {
                            candidate: cin.candidate,
                            sdp_mid: cin.sdp_mid
                        })
                    ));
                }
            }
        }.boxed())).await;

        let ns = negotiation_send.clone();
        handle.on_data_channel(Box::new(move |x| {
            let nss = ns.clone();
            async move {
                info!("never call me");
                let v = x.clone();
                x.on_open(Box::new(move || async move { let _ = nss.unbounded_send(RtcNegotiationNotification::DataChannelOpened(RtcDataChannel::new(v).await)); }.boxed())).await;
            }.boxed()
        })).await;

        Ok(())
    }

    async fn create_channels(&self, builder: &mut RtcPeerConnection) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        self.assign_channel_ids(builder);
        self.accept_connection(builder).await
    }

    async fn accept_connection(&self, builder: &RtcPeerConnection) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        let chans = self.initialize_channels(builder).await?;

        let mut channels = Vec::with_capacity(builder.channels.len());
        
        while channels.len() < builder.channels.len() {
            let p = self.negotiate().await?;
            channels.push(p);
        }

        let mut ret = vec!();

        for bc in &chans {
            let mut r: Vec<RtcDataChannel> = Self::drain_where(&mut channels, |f| Arc::ptr_eq(&f.handle, bc));
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

    async fn initialize_channels(&self, builder: &RtcPeerConnection) -> Result<Vec<Arc<webrtc::data_channel::RTCDataChannel>>, RtcPeerConnectionError> {
        let mut channel_setup = vec!();

        for ch in &builder.channels {
            let dc = self.handle.create_data_channel(ch.label.as_str(), Some(ch.configuration().into())).await.map_err(|x| RtcPeerConnectionError::Creation(format!("{:?}", x)))?;
            let ns = self.negotiation_send.clone();
            let ddc = dc.clone();
            dc.on_open(Box::new(|| async move { let _ = ns.unbounded_send(RtcNegotiationNotification::DataChannelOpened(RtcDataChannel::new(ddc).await)); }.boxed())).await;
            channel_setup.push(dc);
        }

        match builder.attempt {
            RtcPeerConnectionAttempt::Offer => {
                self.send_offer().await?;
            },
            RtcPeerConnectionAttempt::Answer => {}
        };

        Ok(channel_setup)
    }

    async fn send_offer(&self) -> Result<(), RtcPeerConnectionError> {
        let offer = self.handle.create_offer(None).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;
        self.handle.set_local_description(offer.clone()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        self.message_handler.send(RtcNegotiationMessage::RemoteSessionDescription(offer.into())).await?;

        Ok(())
    }

    async fn negotiate(&self) -> Result<RtcDataChannel, RtcPeerConnectionError> {
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
                }
            }
            else if let Some(m) = self.message_handler.receive().await? {
                self.receive_negotiation_message(m, &mut candidate_buffer).await?;
            }

            PollFuture::once().await;
        }
    }

    async fn receive_negotiation_message(&self, message: RtcNegotiationMessage, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        match message {
            RtcNegotiationMessage::RemoteCandidate(c) => {
                if self.has_remote_description().await {
                    self.add_remote_candidate(c).await
                }
                else {
                    candidate_buffer.push(c);
                    Ok(())
                }
            },
            RtcNegotiationMessage::RemoteSessionDescription(c) => {
                if !self.has_remote_description().await {
                    if c.sdp_type == "offer" {
                        self.handle_remote_offer(c, candidate_buffer).await
                    }
                    else {
                        self.handle_remote_answer(c, candidate_buffer).await
                    }
                }
                else {
                    Ok(())
                }
            },
        }
    }

    async fn handle_remote_offer(&self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        let description = c.into();
        self.handle.set_remote_description(description).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        let answer = self.handle.create_answer(None).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;
        self.handle.set_local_description(answer.clone()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        self.message_handler.send(RtcNegotiationMessage::RemoteSessionDescription(answer.into())).await?;

        Ok(())
    }

    async fn handle_remote_answer(&self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        self.handle.set_remote_description(c.into()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        Ok(())
    }

    async fn has_remote_description(&self) -> bool {
        self.handle.remote_description().await.is_some()
    }

    async fn add_remote_candidate(&self, c: RtcIceCandidate) -> Result<(), RtcPeerConnectionError> {
        self.handle.add_ice_candidate(c.into()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))
    }
}

enum RtcNegotiationNotification {
    SendMessage(RtcNegotiationMessage),
    DataChannelOpened(RtcDataChannel)
}


pub struct RtcDataChannel {
    current_send: Cell<Option<tokio::task::JoinHandle<()>>>,
    handle: Arc<webrtc::data_channel::RTCDataChannel>,
    message_recv: RefCell<mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>>
}

impl RtcDataChannel {
    async fn new(handle: Arc<webrtc::data_channel::RTCDataChannel>) -> Self {
        let (message_send, message_recv) = mpsc::unbounded();
        let message_recv = RefCell::new(message_recv);

        let ms = message_send.clone();
        handle.on_message(Box::new(move |x| {
            info!("mesange receivussy");
            ms.unbounded_send(Ok(x.data.to_vec()));
            async {}.boxed()
        })).await;

        let ms = message_send.clone();
        handle.on_close(Box::new(move || {
            info!("erres");
            ms.unbounded_send(Err(RtcDataChannelError::Receive("Channel closed.".to_string())));
            async {}.boxed()
        })).await;

        let ms = message_send.clone();
        handle.on_error(Box::new(move |x| {
            info!("erres");
            ms.unbounded_send(Err(RtcDataChannelError::Receive(format!("{}", x))));
            async {}.boxed()
        })).await;

        info!("channels set");

        let current_send = Cell::new(None);

        Self { current_send, handle, message_recv }
    }

    pub fn id(&self) -> u16 {
        self.handle.id()
    }

    pub fn label(&self) -> &str {
        self.handle.label()
    }

    pub fn ready_state(&self) -> RtcDataChannelReadyState {
        self.handle.ready_state().into()
    }

    pub fn send(&self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        block_await(self.handle.send(&message.to_vec().into())).map_err(|x| RtcDataChannelError::Send(format!("{:?}", x)))?;

        Ok(())
    }

    pub fn receive(&self, buffer: &mut impl std::io::Write) -> Result<Option<usize>, RtcDataChannelError> {
        match self.message_recv.borrow_mut().try_next() {
            Ok(Some(x)) => { let b = x?; buffer.write(&b[..]); Ok(Some(b.len())) },
            Err(_) => Ok(None),
            _ => unimplemented!()
        }
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
        match self.message_recv.borrow_mut().try_next() {
            Ok(Some(x)) => x.map(|q| Some(q)),
            Err(_) => Ok(None),
            _ => unimplemented!()
        }
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

pub struct PollFuture {
    count: u32
}

impl PollFuture {
    pub fn new(count: u32) -> PollFuture {
        PollFuture { count }
    }

    pub fn done() -> PollFuture {
        PollFuture::new(0)
    }

    pub fn once() -> PollFuture {
        PollFuture::new(1)
    }
}

impl Future for PollFuture {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.count > 0 {
            self.get_mut().count -= 1;
            return Poll::Pending;
        }

        Poll::Ready(())
    }
}

impl From<&RtcDataChannelConfiguration> for webrtc::data_channel::data_channel_init::RTCDataChannelInit {
    fn from(x: &RtcDataChannelConfiguration) -> Self {
        let mut ret = webrtc::data_channel::data_channel_init::RTCDataChannelInit::default();

        if let Some(i) = x.max_packet_life_time { ret.max_packet_life_time = Some(i); }
        if let Some(i) = x.max_retransmits { ret.max_retransmits = Some(i); }
        if let Some(i) = &x.protocol { ret.protocol = Some(i.clone()); }
        if let Some(i) = x.negotiated { ret.negotiated = Some(i); }
        if let Some(i) = x.id { ret.id = Some(i); }

        ret
    }
}

impl From<webrtc::peer_connection::sdp::session_description::RTCSessionDescription> for RtcSessionDescription {
    fn from(x: webrtc::peer_connection::sdp::session_description::RTCSessionDescription) -> Self {
        RtcSessionDescription {
            sdp: x.sdp.to_string(),
            sdp_type: x.sdp_type.to_string()
        }
    }
}

impl From<RtcSessionDescription> for webrtc::peer_connection::sdp::session_description::RTCSessionDescription {
    fn from(x: RtcSessionDescription) -> Self {
        let mut ret = Self::default();

        ret.sdp = x.sdp.to_string();
        ret.sdp_type =  x.sdp_type.as_str().into();

        ret
    }
}


use webrtc::ice_transport::ice_server::RTCIceServer;

impl From<RtcIceCandidate> for webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
    fn from(x: RtcIceCandidate) -> Self {
        let mut res = Self::default();
        res.candidate = x.candidate;
        res.sdp_mid = x.sdp_mid;
        res
    }
}

impl From<&RtcConfiguration> for webrtc::peer_connection::configuration::RTCConfiguration {
    fn from(x: &RtcConfiguration) -> Self {
        let mut ret = Self::default();
        ret.ice_servers = x.ice_servers.iter().map(|x| webrtc::ice_transport::ice_server::RTCIceServer { urls: vec!(x.to_string()), ..Default::default() }).collect();
        ret.ice_transport_policy = x.ice_transport_policy.into();

        ret
    }
}

impl From<RtcIceTransportPolicy> for webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy {
    fn from(x: RtcIceTransportPolicy) -> Self {
        match x {
            RtcIceTransportPolicy::All => Self::All,
            RtcIceTransportPolicy::Relay => Self::Relay
        }
    }
}

pub fn block_await<T, F: Future<Output = T> + FutureExt>(fut: F) -> T {
    use std::future::*;

    let wake = dummy_waker::dummy_waker();
    let mut ctx = Context::from_waker(&wake);

    let mut boxed = Box::pin(fut);

    let mut pll = Poll::Pending;
    while let Poll::Pending = pll {
        pll = Future::poll(std::pin::Pin::as_mut(&mut boxed), &mut ctx);
    }

    match pll {
        Poll::Pending => unreachable!(),
        Poll::Ready(x) => x
    }
}

impl From<webrtc::data_channel::data_channel_state::RTCDataChannelState> for  RtcDataChannelReadyState {
    fn from(x: webrtc::data_channel::data_channel_state::RTCDataChannelState) -> Self {
        match x {
            webrtc::data_channel::data_channel_state::RTCDataChannelState::Open => RtcDataChannelReadyState::Open,
            webrtc::data_channel::data_channel_state::RTCDataChannelState::Closed => RtcDataChannelReadyState::Closed,
            webrtc::data_channel::data_channel_state::RTCDataChannelState::Connecting => RtcDataChannelReadyState::Connecting,
            webrtc::data_channel::data_channel_state::RTCDataChannelState::Closing => RtcDataChannelReadyState::Closing,
            _ => unimplemented!()
        }
    }
}