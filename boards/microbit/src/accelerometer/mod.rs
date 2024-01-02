//! Accelerometer for the micro:bit
pub use lsm303agr::AccelOutputDataRate;
use {
    embassy_nrf::{
        interrupt::typelevel::{Binding, SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0},
        peripherals::{P0_08, P0_16, TWISPI0},
        twim::{self, InterruptHandler},
        Peripheral,
    },
    embassy_sync::channel::DynamicSender,
    embassy_time::{Delay, Duration, Ticker},
    lsm303agr::{
        interface::I2cInterface, mode::MagOneShot, AccelMode, Error as LsmError, Lsm303agr, Status,
    },
};

type I2C<'d> = twim::Twim<'d, TWISPI0>;

/// Accelerometer error
pub type Error = LsmError<twim::Error, ()>;

/// Accelerometer peripheral present on the microbit
pub struct Accelerometer<'d> {
    sensor: Lsm303agr<I2cInterface<I2C<'d>>, MagOneShot>,
}

/// Backward-compatibility hack for lsm303agr accel data.
pub struct Measurement {
    /// x-axis acceleration in milli-g
    pub x: i32, 
    /// y-axis acceleration in milli-g
    pub y: i32, 
    /// z-axis acceleration in milli-g
    pub z: i32, 
}


impl<'d> Accelerometer<'d> {
    /// Create and initialize the accelerometer
    pub fn new(
        twispi0: TWISPI0,
        irq: impl Peripheral<P = SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0> + 'd + Binding<SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0, InterruptHandler<TWISPI0>>,
        sda: P0_16,
        scl: P0_08,
    ) -> Result<Self, Error> {
        let config = twim::Config::default();
        let twi = twim::Twim::new(twispi0, irq, sda, scl, config);

        let mut sensor = Lsm303agr::new_with_i2c(twi);
        sensor.init()?;
        sensor.set_accel_mode_and_odr(&mut Delay, AccelMode::Normal, AccelOutputDataRate::Hz10)?;

        Ok(Self { sensor })
    }

    /// Return status of accelerometer
    pub fn accel_status(&mut self) -> Result<Status, Error> {
        self.sensor.accel_status()
    }

    /// Return accelerometer data
    ///
    /// Returned in mg (milli-g) where 1g is 9.8m/s².
    pub fn accel_data(&mut self) -> Result<Measurement, Error> {
        let acc = self.sensor.acceleration()?;
        let (x, y, z) = acc.xyz_mg();
        Ok ( Measurement { x, y, z } )
    }

    /// Run a continuous task outputing accelerometer data at the configured data rate
    pub async fn run(
        &mut self,
        rate: AccelOutputDataRate,
        sender: DynamicSender<'_, Measurement>,
    ) -> Result<(), Error> {
        let delay = match rate {
            AccelOutputDataRate::Hz1 => Duration::from_millis(1000),
            AccelOutputDataRate::Hz10 => Duration::from_millis(100),
            AccelOutputDataRate::Hz25 => Duration::from_millis(40),
            AccelOutputDataRate::Hz50 => Duration::from_millis(20),
            AccelOutputDataRate::Hz100 => Duration::from_millis(10),
            AccelOutputDataRate::Hz200 => Duration::from_millis(5),
            AccelOutputDataRate::Hz400 => Duration::from_micros(2500),
            AccelOutputDataRate::Khz1_344 => Duration::from_micros(744),
            AccelOutputDataRate::Khz1_620LowPower => Duration::from_micros(617),
            AccelOutputDataRate::Khz5_376LowPower => Duration::from_micros(186),
        };
        let mut ticker = Ticker::every(delay);
        loop {
            ticker.next().await;
            let data = self.accel_data()?;
            let _ = sender.try_send(data);
        }
    }
}
