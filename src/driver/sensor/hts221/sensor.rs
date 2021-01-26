use crate::bind::Bind;
use crate::prelude::*;
use crate::synchronization::Mutex;
use core::fmt::Debug;
use core::ops::Add;
use embedded_hal::blocking::i2c::{Read, Write, WriteRead};
use embedded_hal::digital::v2::InputPin;
use crate::hal::gpio::exti_pin::ExtiPin;
use cortex_m::interrupt::Nr;
use crate::driver::sensor::hts221::ready::{Ready, DataReady};
use crate::driver::sensor::hts221::register::calibration::*;
use core::default::Default;
use crate::driver::sensor::hts221::register::who_am_i::WhoAmI;
use crate::hal::i2c::I2cAddress;
use crate::driver::sensor::hts221::register::status::Status;
use crate::driver::sensor::hts221::register::t_out::Tout;
use crate::driver::sensor::hts221::register::h_out::Hout;
use crate::driver::sensor::hts221::register::ctrl1::{Ctrl1, OutputDataRate, BlockDataUpdate};
use crate::driver::sensor::hts221::register::ctrl2::Ctrl2;
use crate::driver::sensor::hts221::register::ctrl3::Ctrl3;

pub const ADDR: u8 = 0x5F;

pub struct Sensor<I: WriteRead + Read + Write + 'static>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    address: I2cAddress,
    i2c: Option<Address<Mutex<I>>>,
    calibration: Option<Calibration>,
}

impl<I: WriteRead + Read + Write + 'static> Sensor<I>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    pub fn new() -> Self {
        Self {
            address: I2cAddress::new( ADDR ),
            i2c: None,
            calibration: None,
        }
    }

    // ------------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------------

    fn initialize(&'static mut self) -> Completion {
        Completion::defer(async move {
            if let Some(ref i2c) = self.i2c {
                let mut i2c = i2c.lock().await;

                Ctrl2::modify( self.address, &mut i2c, |reg| {
                    reg.boot();
                });

                Ctrl1::modify( self.address, &mut i2c, |reg| {
                    reg.power_active()
                        .output_data_rate( OutputDataRate::Hz1 )
                        .block_data_update( BlockDataUpdate::MsbLsbReading );
                });

                Ctrl3::modify( self.address, &mut i2c, |reg| {
                    reg.enable(true);
                });

                log::info!("[hts221] address=0x{:X}", WhoAmI::read( self.address, &mut i2c) );
                loop {
                    // Ensure status is emptied
                    if ! Status::read( self.address, &mut i2c).any_available() {
                        break
                    }
                    Hout::read(self.address, &mut i2c);
                    Tout::read(self.address, &mut i2c);
                }
            }
        })
    }

    fn start(&'static mut self) -> Completion {
        Completion::defer(async move {
            if let Some(ref i2c) = self.i2c {
                let mut i2c = i2c.lock().await;
                self.calibration.replace(Calibration::read( self.address, &mut i2c));
            }
        })
    }

}

impl<I: WriteRead + Read + Write> Actor for Sensor<I>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    type Event = ();
}

impl<I: WriteRead + Read + Write + 'static> Bind<Mutex<I>> for Sensor<I>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    fn on_bind(&'static mut self, address: Address<Mutex<I>>) {
        self.i2c.replace(address);
    }
}

impl<I: WriteRead + Read + Write> NotificationHandler<Lifecycle> for Sensor<I>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    fn on_notification(&'static mut self, event: Lifecycle) -> Completion {
        log::info!("[hts221] Lifecycle: {:?}", event);
        match event {
            Lifecycle::Initialize => { self.initialize() }
            Lifecycle::Start => { self.start() }
            Lifecycle::Stop => { Completion::immediate() }
            Lifecycle::Sleep => { Completion::immediate() }
            Lifecycle::Hibernate => { Completion::immediate() }
        }
    }
}

impl<I: WriteRead + Read + Write> NotificationHandler<DataReady> for Sensor<I>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    fn on_notification(&'static mut self, message: DataReady) -> Completion {
        Completion::defer(async move {
            if self.i2c.is_some() {
                let mut i2c = self.i2c.as_ref().unwrap().lock().await;

                if let Some(ref calibration) = self.calibration {
                    let t_out = Tout::read(self.address, &mut i2c);
                    let t = calibration.calibrated_temperature( t_out );

                    let h_out = Hout::read(self.address, &mut i2c);
                    let h = calibration.calibrated_humidity( h_out );

                    log::info!("[hts221] temperature={:.2}°F humidity={:.2}%rh", t.into_fahrenheit(), h);
                } else {
                    log::info!("[hts221] no calibration data available")
                }
            }
        })
    }
}


impl<I: WriteRead + Read + Write + 'static> Address<Sensor<I>>
    where
        <I as WriteRead>::Error: Debug,
        <I as Write>::Error: Debug,
{
    pub fn signal_data_ready(&self) {
        self.notify(DataReady)
    }
}