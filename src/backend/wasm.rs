use crate::*;

use futures::channel::*;
use futures::channel::mpsc::UnboundedSender;
use std::cell::*;
use std::collections::*;
use std::rc::*;
use wasm_bindgen::*;
use wasm_bindgen::closure::*;
use wasm_bindgen_futures::*;

pub struct RtcPeerConnector {
    handle: web_sys::RtcPeerConnection,
    event_handlers: RtcPeerConnectionEventHandlers,
    message_handler: Box<dyn RtcMessageHandler>,
    negotiation_recv: RefCell<mpsc::UnboundedReceiver<RtcNegotiationNotification>>
}

impl RtcPeerConnector {
    pub async fn connect(mut builder: RtcPeerConnection, message_handler: Box<dyn RtcMessageHandler>) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        let handle = web_sys::RtcPeerConnection::new_with_configuration(&builder.configuration().into()).map_err(|x| RtcPeerConnectionError::Creation(format!("{:?}", x)))?;
        let (negotiation_send, negotiation_recv) = mpsc::unbounded();
        let negotiation_recv = RefCell::new(negotiation_recv);
        let event_handlers = RtcPeerConnectionEventHandlers::new(&handle, negotiation_send);    

        Self { handle, event_handlers, message_handler, negotiation_recv }.create_channels(&mut builder).await
    }

    async fn create_channels(&self, builder: &mut RtcPeerConnection) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        self.assign_channel_ids(builder)?;
        let _ = self.initialize_channels(builder).await?;
        self.accept_connection(builder).await
    }

    async fn accept_connection(&self, builder: &RtcPeerConnection) -> Result<Vec<RtcDataChannel>, RtcPeerConnectionError> {
        let mut channels = Vec::with_capacity(builder.channels.len());
        
        while channels.len() < builder.channels.len() {
            let p = self.negotiate().await?;
            channels.push(p);
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

    async fn initialize_channels(&self, builder: &RtcPeerConnection) -> Result<Vec<web_sys::RtcDataChannel>, RtcPeerConnectionError> {
        let mut channel_setup = vec!();

        for ch in &builder.channels {
            let dc = self.handle.create_data_channel_with_data_channel_dict(ch.label.as_str(), &ch.configuration().into());
            dc.set_onopen(Some(self.event_handlers.on_data_channel_open.as_ref().as_ref().unchecked_ref()));
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
        let offer = JsFuture::from(self.handle.create_offer_with_rtc_offer_options(&web_sys::RtcOfferOptions::new())).await
            .map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?.unchecked_into::<web_sys::RtcSessionDescription>();
        JsFuture::from(self.handle.set_local_description(&offer.clone().unchecked_into())).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        self.message_handler.send(RtcNegotiationMessage::RemoteSessionDescription(RtcSessionDescription {
            sdp_type: match offer.type_() {
                web_sys::RtcSdpType::Answer => "answer",
                web_sys::RtcSdpType::Offer => "offer",
                web_sys::RtcSdpType::Pranswer => "pranswer",
                web_sys::RtcSdpType::Rollback => "rollback",
                _ => Err(RtcPeerConnectionError::Negotiation("Unexpected SDP type".to_string()))?
            }.to_string(),
            sdp: offer.sdp(),
        })).await?;

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
                    RtcNegotiationNotification::Failed(m) => return Err(m)
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
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
        description.sdp(&c.sdp);
        JsFuture::from(self.handle.set_remote_description(&description)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        let answer = JsFuture::from(self.handle.create_answer()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;
        let answer_sdp = js_sys::Reflect::get(&answer, &JsValue::from_str("sdp")).unwrap().as_string().unwrap();
        let mut answer_obj = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
        answer_obj.sdp(&answer_sdp);
        JsFuture::from(self.handle.set_local_description(&answer_obj)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;
        self.message_handler.send(RtcNegotiationMessage::RemoteSessionDescription(RtcSessionDescription {
            sdp_type: "answer".into(),
            sdp: answer_sdp,
        })).await?;

        Ok(())
    }

    async fn handle_remote_answer(&self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::from_js_value(&JsValue::from_str(&c.sdp_type)).unwrap());
        description.sdp(&c.sdp);
        JsFuture::from(self.handle.set_remote_description(&description)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        Ok(())
    }

    fn has_remote_description(&self) -> bool {
        self.handle.remote_description().is_some()
    }

    async fn add_remote_candidate(&self, c: RtcIceCandidate) -> Result<(), RtcPeerConnectionError> {
        let mut cand = web_sys::RtcIceCandidateInit::new(&c.candidate);
        cand.sdp_mid(Some(&c.sdp_mid));
        JsFuture::from(self.handle.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand))).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        Ok(())
    }
}

enum RtcNegotiationNotification {
    SendMessage(RtcNegotiationMessage),
    DataChannelOpened(RtcDataChannel),
    Failed(RtcPeerConnectionError)
}

struct RtcPeerConnectionEventHandlers {
    handle: web_sys::RtcPeerConnection,
    _on_data_channel: Closure<dyn FnMut(web_sys::RtcDataChannelEvent)>,
    on_data_channel_open: Rc<Closure<dyn FnMut(web_sys::RtcDataChannelEvent)>>,
    _on_ice_candidate: Closure<dyn FnMut(web_sys::RtcPeerConnectionIceEvent)>,
    _on_ice_connection_state_change: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
}

impl RtcPeerConnectionEventHandlers {
    pub fn new(handle: &web_sys::RtcPeerConnection, negotiation_send: mpsc::UnboundedSender<RtcNegotiationNotification>) -> Self {
        let handle = handle.clone();

        let ns = negotiation_send.clone();
        let _on_ice_candidate = Closure::wrap(Box::new(move |ev: web_sys::RtcPeerConnectionIceEvent| {
            if let Some(candidate) = ev.candidate() {
                let _ = ns.unbounded_send(RtcNegotiationNotification::SendMessage(
                    RtcNegotiationMessage::RemoteCandidate(RtcIceCandidate {
                        candidate: candidate.candidate(),
                        sdp_mid: candidate.sdp_mid().unwrap_or("".to_string())
                    })
                ));
            }
            else {
                let _ = ns.unbounded_send(RtcNegotiationNotification::SendMessage(
                    RtcNegotiationMessage::RemoteCandidate(RtcIceCandidate {
                        candidate: "".to_string(),
                        sdp_mid: "".to_string()
                    })
                ));
            }
        }) as Box<dyn FnMut(web_sys::RtcPeerConnectionIceEvent)>);
        handle.set_onicecandidate(Some(_on_ice_candidate.as_ref().unchecked_ref()));

        let ns = negotiation_send.clone();
        let on_data_channel_open = Rc::new(Closure::wrap(Box::new(move |ev: web_sys::RtcDataChannelEvent| {
            let chan;
            if ev.channel().is_undefined() {
                chan = ev.target().unwrap().dyn_into::<web_sys::RtcDataChannel>().unwrap();
            }
            else {
                chan = ev.channel();
            }

            let _ = ns.unbounded_send(RtcNegotiationNotification::DataChannelOpened(RtcDataChannel::new(chan)));
        }) as Box<dyn FnMut(web_sys::RtcDataChannelEvent)>));

        let r = on_data_channel_open.clone();
        let _on_data_channel = Closure::wrap(Box::new(move |ev: web_sys::RtcDataChannelEvent| {
            ev.channel().set_onopen(Some(r.as_ref().as_ref().unchecked_ref()));
        }) as Box<dyn FnMut(web_sys::RtcDataChannelEvent)>);
        handle.set_ondatachannel(Some(_on_data_channel.as_ref().unchecked_ref()));

        let han = handle.clone();
        let ns = negotiation_send.clone();
        let _on_ice_connection_state_change = Closure::wrap(Box::new(move |_: wasm_bindgen::JsValue| {
            let evs = js_sys::Reflect::get(&han, &js_sys::JsString::from("connectionState")).map(|x| x.as_string().unwrap_or("".to_string())).unwrap_or("".to_string());
            if evs.as_str() == "failed" {
                let _ = ns.unbounded_send(RtcNegotiationNotification::Failed(RtcPeerConnectionError::IceNegotiationFailure("ICE protocol could not find a valid candidate pair.".to_string())));
            }
        }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);
        js_sys::Reflect::set(&handle, &js_sys::JsString::from("onconnectionstatechange"), _on_ice_connection_state_change.as_ref().unchecked_ref()).unwrap();

        Self { handle, _on_data_channel, on_data_channel_open, _on_ice_candidate, _on_ice_connection_state_change }
    }
}

impl Drop for RtcPeerConnectionEventHandlers {
    fn drop(&mut self) {
        self.handle.set_onicecandidate(None);
        self.handle.set_ondatachannel(None);
    }
}


pub struct RtcDataChannel {
    handle: web_sys::RtcDataChannel,
    _event_handlers: RtcDataChannelEventHandlers,
    label: String,
    message_recv: RefCell<mpsc::UnboundedReceiver<Result<Vec<u8>, RtcDataChannelError>>>
}

impl RtcDataChannel {
    fn new(handle: web_sys::RtcDataChannel) -> Self {
        handle.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        let label = handle.label();
        let (message_send, message_recv) = mpsc::unbounded();
        let message_recv = RefCell::new(message_recv);
        let _event_handlers = RtcDataChannelEventHandlers::new(&handle, message_send);

        Self { handle, _event_handlers, label, message_recv }
    }

    pub fn id(&self) -> u16 {
        js_sys::Reflect::get(&self.handle, &js_sys::JsString::from("id".to_string())).unwrap().as_f64().unwrap() as u16
    }

    pub fn label(&self) -> &str {
        self.label.as_str()
    }

    pub fn ready_state(&self) -> RtcDataChannelReadyState {
        match js_sys::Reflect::get(&self.handle, &js_sys::JsString::from("readyState".to_string())).unwrap().as_string().unwrap().as_str() {
            "connecting" => RtcDataChannelReadyState::Connecting,
            "open" => RtcDataChannelReadyState::Open,
            "closing" => RtcDataChannelReadyState::Closing,
            "closed" => RtcDataChannelReadyState::Closed,
            _ => unimplemented!()
        }
    }

    pub fn send(&self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        let ab = js_sys::ArrayBuffer::new(message.len().try_into().unwrap());
        let arr = js_sys::Uint8Array::new(&ab);
        arr.copy_from(message);

        self.handle.send_with_array_buffer(&ab).map_err(|x| RtcDataChannelError::Send(format!("{:?}", x)))?;
        Ok(())
    }

    pub fn receive(&self, buffer: &mut impl std::io::Write) -> Result<Option<usize>, RtcDataChannelError> {
        match self.message_recv.borrow_mut().try_next() {
            Ok(Some(x)) => { let b = x?; buffer.write(&b[..]).map_err(|x| RtcDataChannelError::WriteError(x))?; Ok(Some(b.len())) },
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

impl Drop for RtcDataChannel {
    fn drop(&mut self) {
        self.handle.close();
    }
}

struct RtcDataChannelEventHandlers {
    handle: web_sys::RtcDataChannel,
    _on_close: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl RtcDataChannelEventHandlers {
    pub fn new(handle: &web_sys::RtcDataChannel, message_send: UnboundedSender<Result<Vec<u8>, RtcDataChannelError>>) -> Self {
        let handle = handle.clone();

        handle.set_onopen(None);

        let ns = message_send.clone();
        let _on_message = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
            let _ = ns.unbounded_send(match ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                Ok(data) => Ok(js_sys::Uint8Array::new(&data).to_vec()),
                Err(data) => Err(RtcDataChannelError::Receive(format!("{:?}", data))),
            });
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);
        handle.set_onmessage(Some(_on_message.as_ref().unchecked_ref()));

        let ns = message_send.clone();
        let _on_close = Closure::wrap(Box::new(move |ev: wasm_bindgen::JsValue| {
            let _ = ns.unbounded_send(Err(RtcDataChannelError::Receive(format!("{:?}", ev))));
        }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);
        handle.set_onclose(Some(_on_close.as_ref().unchecked_ref()));
        handle.set_onerror(Some(_on_close.as_ref().unchecked_ref()));

        Self { handle, _on_close, _on_message }
    }
}

impl Drop for RtcDataChannelEventHandlers {
    fn drop(&mut self) {
        self.handle.set_onmessage(None);
        self.handle.set_onclose(None);
        self.handle.set_onerror(None);
    }
}

impl From<&RtcConfiguration> for web_sys::RtcConfiguration {
    fn from(config: &RtcConfiguration) -> Self {
        let ice_servers = js_sys::Array::new();
        for s in &config.ice_servers {
            let urls = js_sys::Array::new();

            for u in &s.urls {
                urls.push(&js_sys::JsString::from(u.as_str()));
            }

            let mut ice_server = web_sys::RtcIceServer::new();
            ice_server.urls(&urls);
            
            if let Some(u) = &s.username {
                ice_server.username(u.as_str());
            }

            if let Some(u) = &s.credential {
                ice_server.credential(u.as_str());
            }

            ice_servers.push(&ice_server);
        }

        let mut ret = web_sys::RtcConfiguration::new();
        ret.ice_servers(&ice_servers);
        ret.ice_transport_policy(config.ice_transport_policy.into());

        ret
    }
}

impl From<RtcIceTransportPolicy> for web_sys::RtcIceTransportPolicy {
    fn from(x: RtcIceTransportPolicy) -> Self {
        match x {
            RtcIceTransportPolicy::All => web_sys::RtcIceTransportPolicy::All,
            RtcIceTransportPolicy::Relay => web_sys::RtcIceTransportPolicy::Relay
        }
    }
}

impl From<web_sys::RtcIceConnectionState> for RtcIceConnectionState {
    fn from(x: web_sys::RtcIceConnectionState) -> Self {
        match x {
            web_sys::RtcIceConnectionState::New => RtcIceConnectionState::New,
            web_sys::RtcIceConnectionState::Checking => RtcIceConnectionState::Checking,
            web_sys::RtcIceConnectionState::Connected => RtcIceConnectionState::Connected,
            web_sys::RtcIceConnectionState::Completed => RtcIceConnectionState::Completed,
            web_sys::RtcIceConnectionState::Failed => RtcIceConnectionState::Failed,
            web_sys::RtcIceConnectionState::Disconnected => RtcIceConnectionState::Disconnected,
            web_sys::RtcIceConnectionState::Closed => RtcIceConnectionState::Closed,
            _ => unimplemented!()
        }
    }
}

impl From<web_sys::RtcSignalingState> for RtcSignalingState {
    fn from(x: web_sys::RtcSignalingState) -> Self {
        match x {
            web_sys::RtcSignalingState::Stable => RtcSignalingState::Stable,
            web_sys::RtcSignalingState::Closed => RtcSignalingState::Closed,
            web_sys::RtcSignalingState::HaveLocalOffer => RtcSignalingState::HaveLocalOffer,
            web_sys::RtcSignalingState::HaveLocalPranswer => RtcSignalingState::HaveLocalPranswer,
            web_sys::RtcSignalingState::HaveRemoteOffer => RtcSignalingState::HaveRemoteOffer,
            web_sys::RtcSignalingState::HaveRemotePranswer => RtcSignalingState::HaveRemotePranswer,
            _ => unimplemented!()
        }
    }
}

impl From<&RtcDataChannelConfiguration> for web_sys::RtcDataChannelInit {
    fn from(x: &RtcDataChannelConfiguration) -> Self {
        let mut ret = web_sys::RtcDataChannelInit::new();
        
        if let Some(i) = x.max_packet_life_time { ret.max_packet_life_time(i); }
        if let Some(i) = x.max_retransmits { ret.max_retransmits(i); }
        if let Some(i) = &x.protocol { ret.protocol(i.as_str()); }
        if let Some(i) = x.negotiated { ret.negotiated(i); }
        if let Some(i) = x.id { ret.id(i); }

        ret
    }
}