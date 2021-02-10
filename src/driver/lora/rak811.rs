use crate::domain::time::duration::Milliseconds;
use crate::driver::lora::*;
use crate::driver::timer;
use crate::driver::uart::dma;
use crate::hal::timer::Timer as HalTimer;
use crate::hal::uart::{DmaUart, Error as UartError};
use crate::handler::{RequestHandler, Response};
use crate::prelude::*;

use core::cell::{RefCell, UnsafeCell};

use drogue_rak811::{
    Buffer, Command, ConfigOption, DriverError, EventCode, Response as RakResponse,
};
use embedded_hal::digital::v2::OutputPin;
use heapless::{
    consts,
    spsc::{Consumer, Producer, Queue},
    String,
};

type Uart<U, T> = <dma::Uart<U, T> as Package>::Primary;
type Timer<T> = <timer::Timer<T> as Package>::Primary;

pub struct Rak811Actor<U, T, RST>
where
    U: DmaUart + 'static,
    T: HalTimer + 'static,
    RST: OutputPin + 'static,
{
    uart: Option<Address<Uart<U, T>>>,
    timer: Option<Address<Timer<T>>>,
    command_buffer: String<consts::U128>,
    config: LoraConfig,
    rst: RST,
    rxc: Option<RefCell<Consumer<'static, RakResponse, consts::U8>>>,
}
pub struct Rak811Ingress<U, T>
where
    U: DmaUart + 'static,
    T: HalTimer + 'static,
{
    uart: Option<Address<Uart<U, T>>>,
    timer: Option<Address<Timer<T>>>,
    parse_buffer: Buffer,
    rxp: Option<RefCell<Producer<'static, RakResponse, consts::U8>>>,
}

pub struct Rak811<U, T, RST>
where
    U: DmaUart + 'static,
    T: HalTimer + 'static,
    RST: OutputPin + 'static,
{
    actor: ActorContext<Rak811Actor<U, T, RST>>,
    ingress: ActorContext<Rak811Ingress<U, T>>,
    rxq: UnsafeCell<Queue<RakResponse, consts::U8>>,
}

impl<U, T, RST> Package for Rak811<U, T, RST>
where
    U: DmaUart + 'static,
    T: HalTimer + 'static,
    RST: OutputPin + 'static,
{
    type Primary = Rak811Actor<U, T, RST>;
    type Configuration = (Address<Uart<U, T>>, Address<Timer<T>>);
    fn mount(
        &'static self,
        config: Self::Configuration,
        supervisor: &mut Supervisor,
    ) -> Address<Self::Primary>
    where
        Self: 'static,
    {
        /*
        let mut queue = self.rxq.borrow_mut();
        let (prod, cons): (
            Producer<'static, RakResponse, consts::U8>,
            Consumer<'static, RakResponse, consts::U8>,
        ) = queue.split();*/
        let (prod, cons) = unsafe { (&mut *self.rxq.get()).split() };
        let addr = self.actor.mount((cons, config.0, config.1), supervisor);
        self.ingress.mount((prod, config.0, config.1), supervisor);

        addr
    }
}

impl<U, T, RST> Rak811<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    pub fn new(rst: RST) -> Self {
        Self {
            actor: ActorContext::new(Rak811Actor::new(rst)),
            ingress: ActorContext::new(Rak811Ingress::new()),
            rxq: UnsafeCell::new(Queue::new()),
        }
    }
}

impl<U, T, RST> Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    pub fn new(rst: RST) -> Self {
        Self {
            uart: None,
            timer: None,
            command_buffer: String::new(),
            config: LoraConfig::new(),
            rst,
            rxc: None,
        }
    }

    async fn send_command<'b>(&mut self, command: Command<'b>) -> Result<RakResponse, LoraError> {
        let s = &mut self.command_buffer;
        s.clear();
        command.encode(s);
        s.push_str("\r\n").unwrap();

        log::debug!("Sending command {}", s.as_str());
        let uart = self.uart.as_ref().unwrap();

        uart.write(s.as_bytes()).await?;

        self.recv_response().await
    }

    async fn recv_response(&mut self) -> Result<RakResponse, LoraError>
where {
        loop {
            // Run processing to increase likelyhood we have something to parse.
            if let Some(response) = self.rxc.as_ref().unwrap().borrow_mut().dequeue() {
                return Ok(response);
            }
            self.timer.as_ref().unwrap().delay(Milliseconds(100)).await;
        }
    }

    async fn send_command_ok<'b>(&mut self, command: Command<'b>) -> Result<(), LoraError> {
        let response = self.send_command(command).await;
        match response {
            Ok(RakResponse::Ok) => Ok(()),
            Ok(r) => Err(LoraError::OtherError),
            Err(e) => Err(e.into()),
        }
    }

    async fn apply_config(&mut self, config: LoraConfig) -> Result<(), LoraError> {
        log::debug!("Applying config: {:?}", config);
        if let Some(band) = config.band {
            if self.config.band != config.band {
                self.send_command_ok(Command::SetBand(band)).await?;
                self.config.band.replace(band);
            }
        }
        if let Some(lora_mode) = config.lora_mode {
            if self.config.lora_mode != config.lora_mode {
                self.send_command_ok(Command::SetMode(lora_mode)).await?;
                self.config.lora_mode.replace(lora_mode);
            }
        }

        if let Some(ref device_address) = config.device_address {
            self.send_command_ok(Command::SetConfig(ConfigOption::DevAddr(device_address)))
                .await?;
            self.config.device_address.replace(*device_address);
        }

        if let Some(ref device_eui) = config.device_eui {
            self.send_command_ok(Command::SetConfig(ConfigOption::DevEui(device_eui)))
                .await?;
            self.config.device_eui.replace(*device_eui);
        }

        if let Some(ref app_eui) = config.app_eui {
            self.send_command_ok(Command::SetConfig(ConfigOption::AppEui(app_eui)))
                .await?;
            self.config.app_eui.replace(*app_eui);
        }

        if let Some(ref app_key) = config.app_key {
            self.send_command_ok(Command::SetConfig(ConfigOption::AppKey(app_key)))
                .await?;
            self.config.app_key.replace(*app_key);
        }

        Ok(())
    }
}

impl<U, T, RST> Actor for Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    type Configuration = (
        Consumer<'static, RakResponse, consts::U8>,
        Address<Uart<U, T>>,
        Address<Timer<T>>,
    );
    fn on_mount(&mut self, _: Address<Self>, config: Self::Configuration) {
        self.rxc.replace(RefCell::new(config.0));
        self.uart.replace(config.1);
        self.timer.replace(config.2);
    }
}

impl<U, T, RST> RequestHandler<Initialize> for Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    type Response = Result<(), LoraError>;
    fn on_request(mut self, message: Initialize) -> Response<Self, Self::Response> {
        Response::defer(async move {
            self.rst.set_high().ok();
            self.rst.set_low().ok();
            let response = self.recv_response().await;
            let result = match response {
                Ok(RakResponse::Initialized(band)) => {
                    self.config.band.replace(band);
                    Ok(())
                }
                _ => Err(LoraError::NotInitialized),
            };
            (self, result)
        })
    }
}

impl<U, T, RST> RequestHandler<Reset> for Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    type Response = Result<(), LoraError>;
    fn on_request(mut self, message: Reset) -> Response<Self, Self::Response> {
        Response::defer(async move {
            let response = self.send_command(Command::Reset(message.0)).await;
            let result = match response {
                Ok(RakResponse::Ok) => {
                    let response = self.recv_response().await;
                    match response {
                        Ok(RakResponse::Initialized(band)) => {
                            self.config.band.replace(band);
                            Ok(())
                        }
                        _ => Err(LoraError::NotInitialized),
                    }
                }
                Ok(r) => Err(LoraError::OtherError),
                Err(e) => Err(e.into()),
            };
            (self, result)
        })
    }
}

impl<'b, U, T, RST> RequestHandler<Configure<'b>> for Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    type Response = Result<(), LoraError>;
    fn on_request(mut self, message: Configure<'b>) -> Response<Self, Self::Response> {
        let config = message.0.clone();
        Response::defer(async move {
            let result = self.apply_config(config).await;
            (self, result)
        })
    }
}

impl<U, T, RST> RequestHandler<Join> for Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    type Response = Result<(), LoraError>;
    fn on_request(mut self, message: Join) -> Response<Self, Self::Response> {
        Response::defer(async move {
            let result = self.send_command_ok(Command::Join(message.0)).await;
            let response = match result {
                Ok(_) => {
                    let response = self.recv_response().await;
                    match response {
                        Ok(RakResponse::Recv(EventCode::JoinedSuccess, _, _, _)) => Ok(()),
                        r => {
                            log::info!("Received response: {:?}", r);
                            Err(LoraError::OtherError)
                        }
                    }
                }
                r => {
                    log::info!("Received response: {:?}", r);
                    Err(LoraError::OtherError)
                }
            };
            (self, response)
        })
    }
}

impl<'b, U, T, RST> RequestHandler<Send<'b>> for Rak811Actor<U, T, RST>
where
    U: DmaUart,
    T: HalTimer,
    RST: OutputPin,
{
    type Response = Result<(), LoraError>;
    fn on_request(self, message: Send<'b>) -> Response<Self, Self::Response> {
        Response::immediate(self, Ok(()))
    }
}

impl<U, T> Rak811Ingress<U, T>
where
    U: DmaUart,
    T: HalTimer,
{
    pub fn new() -> Self {
        Self {
            uart: None,
            timer: None,
            parse_buffer: Buffer::new(),
            rxp: None,
        }
    }

    fn digest(&mut self) -> Result<(), LoraError> {
        let result = self.parse_buffer.parse();
        if let Ok(response) = result {
            if !matches!(response, RakResponse::None) {
                log::debug!("Got response: {:?}", response);
                self.rxp
                    .as_ref()
                    .unwrap()
                    .borrow_mut()
                    .enqueue(response)
                    .map_err(|_| LoraError::ReadError)?;
            }
        }
        Ok(())
    }

    async fn process(&mut self) -> Result<(), LoraError> {
        let uart = self.uart.as_ref().unwrap();
        let mut rx_buf: [u8; 128] = [0; 128];

        let len = uart
            .read_with_timeout(&mut rx_buf[..], Milliseconds(100))
            .await?;

        // log::info!("Read {} bytes", len);
        for b in &mut rx_buf[..len] {
            self.parse_buffer.write(*b).unwrap();
        }

        Ok(())
    }
}

struct ReadData;

impl<U, T> Actor for Rak811Ingress<U, T>
where
    U: DmaUart,
    T: HalTimer,
{
    type Configuration = (
        Producer<'static, RakResponse, consts::U8>,
        Address<Uart<U, T>>,
        Address<Timer<T>>,
    );
    fn on_mount(&mut self, me: Address<Self>, config: Self::Configuration) {
        self.rxp.replace(RefCell::new(config.0));
        self.uart.replace(config.1);
        self.timer.replace(config.2);
        me.notify(ReadData);
    }
}

impl<U, T> NotifyHandler<ReadData> for Rak811Ingress<U, T>
where
    U: DmaUart,
    T: HalTimer,
{
    fn on_notify(mut self, message: ReadData) -> Completion<Self> {
        Completion::defer(async move {
            loop {
                if let Err(e) = self.process().await {
                    log::error!("Error reading data: {:?}", e);
                }

                if let Err(e) = self.digest() {
                    log::error!("Error digesting data");
                }
            }
            self
        })
    }
}

impl core::convert::From<UartError> for LoraError {
    fn from(error: UartError) -> Self {
        log::info!("Convert from UART error {:?}", error);
        match error {
            UartError::TxInProgress
            | UartError::TxBufferTooSmall
            | UartError::TxBufferTooLong
            | UartError::Transmit => LoraError::WriteError,
            UartError::RxInProgress
            | UartError::RxBufferTooSmall
            | UartError::RxBufferTooLong
            | UartError::Receive => LoraError::ReadError,
            _ => LoraError::OtherError,
        }
    }
}

impl core::convert::From<DriverError> for LoraError {
    fn from(error: DriverError) -> Self {
        log::info!("Convert from {:?}", error);
        match error {
            DriverError::NotInitialized => LoraError::NotInitialized,
            DriverError::WriteError => LoraError::WriteError,
            DriverError::ReadError => LoraError::ReadError,
            DriverError::OtherError | DriverError::UnexpectedResponse => LoraError::OtherError,
        }
    }
}
