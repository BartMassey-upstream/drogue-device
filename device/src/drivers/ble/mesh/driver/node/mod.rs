use crate::drivers::ble::mesh::address::UnicastAddress;
use crate::drivers::ble::mesh::composition::ElementsHandler;
use crate::drivers::ble::mesh::config::configuration_manager::ConfigurationManager;
use crate::drivers::ble::mesh::config::network::NetworkKeyHandle;
use crate::drivers::ble::mesh::driver::elements::{AppElementsContext, ElementContext, Elements};
use crate::drivers::ble::mesh::driver::pipeline::Pipeline;
use crate::drivers::ble::mesh::driver::DeviceError;
use crate::drivers::ble::mesh::model::ModelIdentifier;
use crate::drivers::ble::mesh::pdu::access::{AccessMessage, AccessPayload};
use crate::drivers::ble::mesh::provisioning::Capabilities;
use crate::drivers::ble::mesh::storage::Storage;
use crate::drivers::ble::mesh::vault::{StorageVault, Vault};
use crate::drivers::ble::mesh::MESH_BEACON;
use core::cell::UnsafeCell;
use core::cell::{Cell, RefCell};
use core::future::Future;
use embassy::blocking_mutex::kind::Noop;
use embassy::channel::mpsc;
use embassy::channel::mpsc::{Channel, Receiver as ChannelReceiver, Sender as ChannelSender};
use embassy::channel::signal::Signal;
use embassy::time::{Duration, Ticker};
use futures::future::{select, Either};
use futures::{pin_mut, StreamExt};
use heapless::Vec;
use rand_core::{CryptoRng, RngCore};

mod context;

pub trait Transmitter {
    type TransmitFuture<'m>: Future<Output = Result<(), DeviceError>>
    where
        Self: 'm;
    fn transmit_bytes<'m>(&'m self, bytes: &'m [u8]) -> Self::TransmitFuture<'m>;
}

pub trait Receiver {
    type ReceiveFuture<'m>: Future<Output = Result<Vec<u8, 384>, DeviceError>>
    where
        Self: 'm;
    fn receive_bytes<'m>(&'m self) -> Self::ReceiveFuture<'m>;
}

pub(crate) struct OutboundChannel<'a> {
    channel: UnsafeCell<Option<Channel<Noop, AccessMessage, 3>>>,
    sender: UnsafeCell<Option<ChannelSender<'a, Noop, AccessMessage, 3>>>,
    receiver: UnsafeCell<Option<ChannelReceiver<'a, Noop, AccessMessage, 3>>>,
}

impl<'a> OutboundChannel<'a> {
    const fn new() -> Self {
        Self {
            channel: UnsafeCell::new(None),
            sender: UnsafeCell::new(None),
            receiver: UnsafeCell::new(None),
        }
    }

    fn initialize(&self) {
        unsafe { &mut *self.channel.get() }.replace(Channel::new());
        let (sender, receiver) = mpsc::split(unsafe { &mut *self.channel.get() }.as_mut().unwrap());
        unsafe { &mut *self.sender.get() }.replace(sender);
        unsafe { &mut *self.receiver.get() }.replace(receiver);
    }

    async fn send(&self, message: AccessMessage) {
        unsafe {
            if let Some(sender) = &*self.sender.get() {
                sender.send(message).await.ok();
            }
        }
    }

    async fn next(&self) -> Option<AccessMessage> {
        unsafe {
            if let Some(receiver) = &mut *self.receiver.get() {
                receiver.recv().await
            } else {
                None
            }
        }
    }
}

// --

pub struct OutboundPublishMessage {
    pub(crate) element_address: UnicastAddress,
    pub(crate) model_identifier: ModelIdentifier,
    pub(crate) payload: AccessPayload,
}

pub(crate) struct OutboundPublishChannel<'a> {
    channel: UnsafeCell<Option<Channel<Noop, OutboundPublishMessage, 3>>>,
    sender: UnsafeCell<Option<ChannelSender<'a, Noop, OutboundPublishMessage, 3>>>,
    receiver: UnsafeCell<Option<ChannelReceiver<'a, Noop, OutboundPublishMessage, 3>>>,
}

impl<'a> OutboundPublishChannel<'a> {
    const fn new() -> Self {
        Self {
            channel: UnsafeCell::new(None),
            sender: UnsafeCell::new(None),
            receiver: UnsafeCell::new(None),
        }
    }

    fn initialize(&self) {
        unsafe { &mut *self.channel.get() }.replace(Channel::new());
        let (sender, receiver) = mpsc::split(unsafe { &mut *self.channel.get() }.as_mut().unwrap());
        unsafe { &mut *self.sender.get() }.replace(sender);
        unsafe { &mut *self.receiver.get() }.replace(receiver);
    }

    async fn send(&self, message: OutboundPublishMessage) {
        unsafe {
            if let Some(sender) = &*self.sender.get() {
                sender.send(message).await.ok();
            }
        }
    }

    async fn next(&self) -> Option<OutboundPublishMessage> {
        unsafe {
            if let Some(receiver) = &mut *self.receiver.get() {
                receiver.recv().await
            } else {
                None
            }
        }
    }

    fn clone_sender(&self) -> ChannelSender<'a, Noop, OutboundPublishMessage, 3> {
        unsafe { &*self.sender.get() }.as_ref().unwrap().clone()
    }
}
// --

#[derive(Copy, Clone, PartialEq)]
pub enum State {
    Unprovisioned,
    Provisioning,
    Provisioned,
}

pub enum MeshNodeMessage {
    ForceReset,
    Shutdown,
}

pub struct ChannelState<'a> {
    outbound: OutboundChannel<'a>,
    publish_outbound: OutboundPublishChannel<'a>,
}

impl<'a> ChannelState<'a> {
    pub const fn new() -> Self {
        Self {
            outbound: OutboundChannel::new(),
            publish_outbound: OutboundPublishChannel::new(),
        }
    }

    pub fn initialize(&self) {
        self.outbound.initialize();
        self.publish_outbound.initialize();
    }
}

pub struct NodeState<'a> {
    pub channel: ChannelState<'a>,
    pub control: Signal<MeshNodeMessage>,
}

impl<'a> NodeState<'a> {
    pub const fn new() -> Self {
        Self {
            channel: ChannelState::new(),
            control: Signal::new(),
        }
    }
}

pub struct Node<'a, E, TX, RX, S, R>
where
    E: ElementsHandler<'a>,
    TX: Transmitter,
    RX: Receiver,
    S: Storage,
    R: RngCore + CryptoRng,
{
    channel_state: &'a mut ChannelState<'a>,
    control_signal: &'a Signal<MeshNodeMessage>,

    //
    state: Cell<State>,
    //
    transmitter: TX,
    receiver: RX,
    configuration_manager: ConfigurationManager<S>,
    rng: RefCell<R>,
    pipeline: RefCell<Pipeline>,
    //
    pub(crate) elements: Elements<'a, E>,
}

impl<'a, E, TX, RX, S, R> Node<'a, E, TX, RX, S, R>
where
    E: ElementsHandler<'a>,
    TX: Transmitter,
    RX: Receiver,
    S: Storage,
    R: RngCore + CryptoRng,
{
    pub fn new(
        channel_state: &'a mut ChannelState<'a>,
        control_signal: &'a Signal<MeshNodeMessage>,
        app_elements: E,
        capabilities: Capabilities,
        transmitter: TX,
        receiver: RX,
        configuration_manager: ConfigurationManager<S>,
        rng: R,
    ) -> Self {
        Self {
            channel_state,
            control_signal,
            state: Cell::new(State::Unprovisioned),
            transmitter,
            receiver,
            configuration_manager,
            rng: RefCell::new(rng),
            pipeline: RefCell::new(Pipeline::new(capabilities)),
            //
            elements: Elements::new(app_elements),
        }
    }

    pub(crate) fn vault(&self) -> StorageVault<S> {
        StorageVault::new(&self.configuration_manager)
    }

    async fn publish(&self, publish: OutboundPublishMessage) -> Result<(), DeviceError> {
        if let Some(network) = self.configuration_manager.configuration().network() {
            if let Some((network, publication)) =
                network.find_publication(&publish.element_address, &publish.model_identifier)
            {
                if let Some(app_key_details) =
                    network.find_app_key_by_index(&publication.app_key_index)
                {
                    let message = AccessMessage {
                        ttl: publication.publish_ttl,
                        network_key: NetworkKeyHandle::from(network),
                        ivi: 0,
                        nid: network.nid,
                        akf: true,
                        aid: app_key_details.aid,
                        src: publish.element_address,
                        dst: publication.publish_address,
                        payload: publish.payload,
                    };
                    self.pipeline
                        .borrow_mut()
                        .process_outbound(self, &message)
                        .await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    async fn loop_unprovisioned(&self) -> Result<Option<State>, DeviceError> {
        self.transmit_unprovisioned_beacon().await?;

        let receive_fut = self.receiver.receive_bytes();

        let mut ticker = Ticker::every(Duration::from_secs(3));
        let ticker_fut = ticker.next();

        pin_mut!(receive_fut);
        pin_mut!(ticker_fut);

        let result = select(receive_fut, ticker_fut).await;

        match result {
            Either::Left((Ok(msg), _)) => {
                self.pipeline
                    .borrow_mut()
                    .process_inbound(self, &*msg)
                    .await
            }
            Either::Right((_, _)) => {
                self.transmit_unprovisioned_beacon().await?;
                Ok(None)
            }
            _ => {
                // TODO handle this
                Ok(None)
            }
        }
    }

    async fn transmit_unprovisioned_beacon(&self) -> Result<(), DeviceError> {
        let mut adv_data: Vec<u8, 31> = Vec::new();
        adv_data.extend_from_slice(&[20, MESH_BEACON, 0x00]).ok();
        adv_data.extend_from_slice(&self.vault().uuid().0).ok();
        adv_data.extend_from_slice(&[0xa0, 0x40]).ok();

        self.transmitter.transmit_bytes(&*adv_data).await
    }

    async fn loop_provisioning(&self) -> Result<Option<State>, DeviceError> {
        let receive_fut = self.receiver.receive_bytes();
        let mut ticker = Ticker::every(Duration::from_secs(1));
        let ticker_fut = ticker.next();

        pin_mut!(receive_fut);
        pin_mut!(ticker_fut);

        let result = select(receive_fut, ticker_fut).await;

        match result {
            Either::Left((Ok(inbound), _)) => {
                self.pipeline
                    .borrow_mut()
                    .process_inbound(self, &*inbound)
                    .await
            }
            Either::Right((_, _)) => {
                self.pipeline.borrow_mut().try_retransmit(self).await?;
                Ok(None)
            }
            _ => {
                // TODO handle this
                Ok(None)
            }
        }
    }

    async fn loop_provisioned(&self) -> Result<Option<State>, DeviceError> {
        let mut ticker = Ticker::every(Duration::from_millis(250));

        let ack_timeout = ticker.next();
        let receive_fut = self.receiver.receive_bytes();
        let outbound_fut = self.channel_state.outbound.next();
        let publish_outbound_fut = self.channel_state.publish_outbound.next();

        pin_mut!(ack_timeout);
        pin_mut!(receive_fut);
        pin_mut!(outbound_fut);
        pin_mut!(publish_outbound_fut);

        let result = select(
            select(receive_fut, ack_timeout),
            select(outbound_fut, publish_outbound_fut),
        )
        .await;
        match result {
            Either::Left((inner, _)) => match inner {
                Either::Left((Ok(inbound), _)) => {
                    self.pipeline
                        .borrow_mut()
                        .process_inbound(self, &*inbound)
                        .await
                }
                Either::Right((_, _)) => {
                    self.pipeline.borrow_mut().try_retransmit(self).await?;
                    Ok(None)
                }
                _ => Ok(None),
            },
            Either::Right((inner, _)) => match inner {
                Either::Left((Some(outbound), _)) => {
                    self.pipeline
                        .borrow_mut()
                        .process_outbound(self, &outbound)
                        .await?;
                    Ok(None)
                }
                Either::Right((Some(publish), _)) => {
                    self.publish(publish).await?;
                    Ok(None)
                }
                _ => Ok(None),
            },
        }
    }

    async fn do_loop(&self) -> Result<(), DeviceError> {
        let current_state = self.state.get();

        if let Some(next_state) = match current_state {
            State::Unprovisioned => self.loop_unprovisioned().await,
            State::Provisioning => self.loop_provisioning().await,
            State::Provisioned => self.loop_provisioned().await,
        }? {
            if matches!(next_state, State::Provisioned) {
                if !matches!(current_state, State::Provisioned) {
                    // only connect during the first transition.
                    self.connect_elements()
                }
            }
            if next_state != current_state {
                self.state.set(next_state);
                self.pipeline.borrow_mut().state(next_state);
            };
        }
        Ok(())
    }

    fn connect_elements(&self) {
        let ctx = AppElementsContext {
            sender: self.channel_state.publish_outbound.clone_sender(),
            address: self.address().unwrap(),
        };
        self.elements.connect(ctx);
    }

    pub async fn run(&mut self) -> Result<(), DeviceError> {
        let mut rng = self.rng.borrow_mut();
        if let Err(e) = self.configuration_manager.initialize(&mut *rng).await {
            // try again as a force reset
            error!("Error loading configuration {}", e);
            warn!("Unable to load configuration; attempting reset.");
            self.configuration_manager.reset();
            self.configuration_manager.initialize(&mut *rng).await?
        }

        drop(rng);

        #[cfg(feature = "defmt")]
        self.configuration_manager.display_configuration();

        self.channel_state.initialize();

        if self.configuration_manager.is_provisioned() {
            self.state.set(State::Provisioned);
            self.connect_elements();
        }

        self.pipeline.borrow_mut().state(self.state.get());

        loop {
            let loop_fut = self.do_loop();
            let signal_fut = self.control_signal.wait();

            pin_mut!(loop_fut);
            pin_mut!(signal_fut);

            let result = select(loop_fut, signal_fut).await;

            match &result {
                Either::Left((_, _)) => {
                    // normal operation
                }
                Either::Right((control_message, _)) => match control_message {
                    MeshNodeMessage::ForceReset => {
                        self.configuration_manager.node_reset().await;
                    }
                    MeshNodeMessage::Shutdown => {}
                },
            }
        }
    }
}
