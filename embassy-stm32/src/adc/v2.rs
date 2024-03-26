use embassy_futures::yield_now;
use embassy_hal_internal::{into_ref, Peripheral};
use embedded_hal_02::blocking::delay::DelayUs;
use stm32_metapac::adc::vals;

use crate::adc::{Adc, AdcPin, Instance, Resolution, RxDma, SampleTime};
use crate::dma::{dma, Transfer};
use crate::peripherals::ADC1;
use crate::time::Hertz;
const ADC_FREQ: Hertz = crate::rcc::HSI_FREQ;

pub const VDDA_CALIB_MV: u32 = 3300;
pub const ADC_MAX: u32 = (1 << 12) - 1;
pub const VREF_INT: u32 = 1230;

/// Default VREF voltage used for sample conversion to millivolts.
pub const VREF_DEFAULT_MV: u32 = 3300;
/// VREF voltage used for factory calibration of VREFINTCAL register.
pub const VREF_CALIB_MV: u32 = 3300;

/// ADC turn-on time
pub const ADC_POWERUP_TIME_US: u32 = 3;

pub struct VrefInt;
impl AdcPin<ADC1> for VrefInt {}
impl super::SealedAdcPin<ADC1> for VrefInt {
    fn channel(&self) -> u8 {
        17
    }
}

impl<T: Instance> Vref<T> {
    /// The value that vref would be if vdda was at 3000mv
    pub fn calibrated_value(&self) -> u16 {
        ADC_MAX as u16
    }

    pub async fn calibrate(&mut self, adc: &mut Adc<'static, T>) -> Calibration {
        let vref_val = Adc::read(adc, self).await;
        Calibration {
            vref_cal: self.calibrated_value(),
            vref_val,
        }
    }
}

pub struct Calibration {
    vref_cal: u16,
    vref_val: u16,
}

impl Calibration {
    /// The millivolts that the calibration value was measured at
    pub const CALIBRATION_UV: u32 = 3_000_000;

    /// Returns the measured VddA in microvolts (uV)
    pub fn vdda_uv(&self) -> u32 {
        (Self::CALIBRATION_UV * self.vref_cal as u32) / self.vref_val as u32
    }

    /// Returns the measured VddA as an f32
    pub fn vdda_f32(&self) -> f32 {
        (Self::CALIBRATION_UV as f32 / 1_000.0) * (self.vref_cal as f32 / self.vref_val as f32)
    }

    /// Returns a calibrated voltage value as in microvolts (uV)
    pub fn cal_uv(&self, raw: u16, resolution: super::Resolution) -> u32 {
        (self.vdda_uv() / resolution.to_max_count()) * raw as u32
    }

    /// Returns a calibrated voltage value as an f32
    pub fn cal_f32(&self, raw: u16, resolution: super::Resolution) -> f32 {
        raw as f32 * self.vdda_f32() / resolution.to_max_count() as f32
    }
}

impl<T: Instance> Drop for Vref<T> {
    fn drop(&mut self) {
        update_vref::<T>(-1)
    }
}

pub struct Temperature;
impl AdcPin<ADC1> for Temperature {}
impl super::SealedAdcPin<ADC1> for Temperature {
    fn channel(&self) -> u8 {
        cfg_if::cfg_if! {
            if #[cfg(any(stm32f2, stm32f40, stm32f41))] {
                16
            } else {
                18
            }
        }
    }
}

impl<T: Instance> Temperature<T> {
    /// Time needed for temperature sensor readings to stabilize
    pub fn start_time_us() -> u32 {
        10
    }
}

/// The state of a continuously running sampler
#[derive(PartialEq)]
pub enum SamplerState {
    Sampled,
    Stopped,
}

pub struct Vbat;
impl AdcPin<ADC1> for Vbat {}
impl super::SealedAdcPin<ADC1> for Vbat {
    fn channel(&self) -> u8 {
        18
    }
}

enum Prescaler {
    Div2,
    Div4,
    Div6,
    Div8,
}

impl Prescaler {
    fn from_pclk2(freq: Hertz) -> Self {
        // Datasheet for F2 specifies min frequency 0.6 MHz, and max 30 MHz (with VDDA 2.4-3.6V).
        #[cfg(stm32f2)]
        const MAX_FREQUENCY: Hertz = Hertz(30_000_000);
        // Datasheet for both F4 and F7 specifies min frequency 0.6 MHz, typ freq. 30 MHz and max 36 MHz.
        #[cfg(not(stm32f2))]
        const MAX_FREQUENCY: Hertz = Hertz(36_000_000);
        let raw_div = freq.0 / MAX_FREQUENCY.0;
        match raw_div {
            0..=1 => Self::Div2,
            2..=3 => Self::Div4,
            4..=5 => Self::Div6,
            6..=7 => Self::Div8,
            _ => panic!("Selected PCLK2 frequency is too high for ADC with largest possible prescaler."),
        }
    }

    pub fn resolution(&self) -> Resolution {
        match T::regs().cr1().read().res() {
            crate::pac::adc::vals::Res::TWELVEBIT => Resolution::TwelveBit,
            crate::pac::adc::vals::Res::TENBIT => Resolution::TenBit,
            crate::pac::adc::vals::Res::EIGHTBIT => Resolution::EightBit,
            crate::pac::adc::vals::Res::SIXBIT => Resolution::SixBit,
        }
    }
}

impl<'d, T> Adc<'d, T>
where
    T: Instance,
{
    pub fn new(adc: impl Peripheral<P = T> + 'd, delay: &mut impl DelayUs<u32>) -> Self {
        into_ref!(adc);
        T::enable_and_reset();

        let presc = Prescaler::from_pclk2(T::frequency());
        T::common_regs().ccr().modify(|w| w.set_adcpre(presc.adcpre()));
        T::regs().cr2().modify(|reg| {
            reg.set_adon(true);
        });

        delay.delay_us(ADC_POWERUP_TIME_US);

        Self {
            adc,
            sample_time: SampleTime::from_bits(0),
        }
    }

    pub fn set_sample_time(&mut self, sample_time: SampleTime) {
        self.sample_time = sample_time;
    }

    pub async fn read(&mut self, pin: &mut impl AdcPin<T>) -> u16 {
        self.set_channel_sample_sequence(&[pin.channel()]).await;
        self.convert()
    }

    /// Enables internal voltage reference and returns [VrefInt], which can be used in
    /// [Adc::read_internal()] to perform conversion.
    pub fn enable_vref(&self) -> Vref<T> {
        update_vref::<T>(1);

        Vref::<T>(core::marker::PhantomData)
    }

    /// Enables internal temperature sensor and returns [Temperature], which can be used in
    /// [Adc::read_internal()] to perform conversion.
    ///
    /// On STM32F42 and STM32F43 this can not be used together with [Vbat]. If both are enabled,
    /// temperature sensor will return vbat value.
    pub fn enable_temperature(&self) -> Temperature<T> {
        T::common_regs().ccr().modify(|w| w.set_tsvrefe(true));

        Temperature::<T>(core::marker::PhantomData)
    }

    /// Enables vbat input and returns [Vbat], which can be used in
    /// [Adc::read_internal()] to perform conversion.
    // pub fn enable_vbat(&self) -> Vbat {
    //     T::common_regs().ccr().modify(|reg| {
    //         reg.set_vbate(true);
    //     });

    //     Vbat {}
    // }

    pub async fn set_sample_time(&mut self, pin: &mut impl AdcPin<T>, sample_time: SampleTime) {
        if Self::get_channel_sample_time(pin.channel()) != sample_time {
            let was_on = Self::is_on();
            if was_on {
                self.stop_adc().await;
            }
            unsafe {
                Self::set_channel_sample_time(pin.channel(), sample_time);
                trace!(
                    "Set sample time for channel {} to {:?}",
                    pin.channel(),
                    Self::get_channel_sample_time(pin.channel())
                );
            }
            if was_on {
                self.start_adc().await;
            }
            // This will make CI pass, but the struct field is no longer relevant as each channel will have an associated sample time.
            self.sample_time = sample_time;
        }
    }
    /// Sets the channel sample time
    ///
    /// ## SAFETY:
    /// - ADON == 0 i.e ADC must not be enabled when this is called.
    unsafe fn set_channel_sample_time(ch: u8, sample_time: SampleTime) {
        let sample_time = sample_time.into();
        if ch <= 9 {
            T::regs().smpr2().modify(|reg| reg.set_smp(ch as _, sample_time));
        } else {
            T::regs().smpr1().modify(|reg| reg.set_smp((ch - 10) as _, sample_time));
        }
    }

    fn set_channels_sample_time(&mut self, ch: &[u8], sample_time: SampleTime) {
        let ch_iter = ch.iter();
        for idx in ch_iter {
            unsafe {
                Self::set_channel_sample_time(*idx, sample_time);
            }
        }
    }

    fn get_channel_sample_time(ch: u8) -> SampleTime {
        match ch {
            0..=9 => T::regs().smpr2().read().smp(ch as _),
            10..=16 => T::regs().smpr1().read().smp(ch as usize - 10),
            _ => panic!("Invalid channel to sample"),
        }
        .into()
    }
    pub async fn set_sample_sequence(
        &mut self,
        sequence: config::Sequence,
        channel: &mut impl AdcPin<T>,
        sample_time: SampleTime,
    ) {
        let was_on = Self::is_on();
        if !was_on {
            self.start_adc().await;
        }

        //Check the sequence is long enough
        T::regs().sqr1().modify(|r| {
            let prev: config::Sequence = r.l().into();
            trace!("Previous sequence length: {:?}", prev);
            if prev < sequence {
                let new_l: config::Sequence = sequence.into();
                trace!("Setting sequence length to {:?}", new_l);
                r.set_l(sequence.into())
            } else {
                r.set_l(prev.into())
            }
        });

        //Set this GPIO as an analog input.
        channel.set_as_analog();

        //Set the channel in the right sequence field.
        match sequence {
            config::Sequence::One => T::regs().sqr3().modify(|w| w.set_sq(0, channel.channel())),
            config::Sequence::Two => T::regs().sqr3().modify(|w| w.set_sq(1, channel.channel())),
            config::Sequence::Three => T::regs().sqr3().modify(|w| w.set_sq(2, channel.channel())),
            config::Sequence::Four => T::regs().sqr3().modify(|w| w.set_sq(3, channel.channel())),
            config::Sequence::Five => T::regs().sqr3().modify(|w| w.set_sq(4, channel.channel())),
            config::Sequence::Six => T::regs().sqr3().modify(|w| w.set_sq(5, channel.channel())),
            config::Sequence::Seven => T::regs().sqr2().modify(|w| w.set_sq(6, channel.channel())),
            config::Sequence::Eight => T::regs().sqr2().modify(|w| w.set_sq(7, channel.channel())),
            config::Sequence::Nine => T::regs().sqr2().modify(|w| w.set_sq(8, channel.channel())),
            config::Sequence::Ten => T::regs().sqr2().modify(|w| w.set_sq(9, channel.channel())),
            config::Sequence::Eleven => T::regs().sqr2().modify(|w| w.set_sq(10, channel.channel())),
            config::Sequence::Twelve => T::regs().sqr2().modify(|w| w.set_sq(11, channel.channel())),
            config::Sequence::Thirteen => T::regs().sqr1().modify(|w| w.set_sq(12, channel.channel())),
            config::Sequence::Fourteen => T::regs().sqr1().modify(|w| w.set_sq(13, channel.channel())),
            config::Sequence::Fifteen => T::regs().sqr1().modify(|w| w.set_sq(14, channel.channel())),
            config::Sequence::Sixteen => T::regs().sqr1().modify(|w| w.set_sq(15, channel.channel())),
        };

        if !was_on {
            self.stop_adc().await;
        }

        self.set_channels_sample_time(&[channel.channel()], sample_time);
    }
    /// Sets the sequence to sample the ADC. Must be less than  elements.
    pub async fn set_channel_sample_sequence(&self, sequence: &[u8]) {
        assert!(sequence.len() <= 8);
        let was_on = Self::is_on();
        if !was_on {
            self.start_adc().await;
        }
        // trace!("Sequence Length: {}", sequence.len());
        let mut iter = sequence.iter();

        T::regs().sqr1().modify(|w| w.set_l((sequence.len() - 1) as _));
        for (idx, ch) in iter.by_ref().take(6).enumerate() {
            T::regs().sqr3().modify(|w| w.set_sq(idx, *ch));
        }
        for (idx, ch) in iter.by_ref().take(6).enumerate() {
            T::regs().sqr2().modify(|w| w.set_sq(idx, *ch));
        }
        for (idx, ch) in iter.by_ref().take(4).enumerate() {
            T::regs().sqr1().modify(|w| w.set_sq(idx, *ch));
        }

        if !was_on {
            self.stop_adc().await;
        }
    }

    fn get_res_clks(res: Resolution) -> u32 {
        match res {
            Resolution::TwelveBit => 12,
            Resolution::TenBit => 11,
            Resolution::EightBit => 9,
            Resolution::SixBit => 7,
        }
    }

    fn get_sample_time_clks(sample_time: SampleTime) -> u32 {
        match sample_time {
            SampleTime::Cycles3 => 3,
            SampleTime::Cycles15 => 15,
            SampleTime::Cycles28 => 28,
            SampleTime::Cycles56 => 56,
            SampleTime::Cycles84 => 84,
            SampleTime::Cycles112 => 112,
            SampleTime::Cycles144 => 144,
            SampleTime::Cycles480 => 480,
        }
    }

    pub fn sample_time_for_us(&self, us: u32) -> SampleTime {
        let res_clks = Self::get_res_clks(self.resolution());
        let us_clks = us * Self::freq().0 / 1_000_000;
        let clks = us_clks.saturating_sub(res_clks);
        match clks {
            0..=3 => SampleTime::Cycles3,
            4..=15 => SampleTime::Cycles15,
            16..=28 => SampleTime::Cycles28,
            29..=56 => SampleTime::Cycles56,
            57..=84 => SampleTime::Cycles84,
            85..=112 => SampleTime::Cycles112,
            113..=144 => SampleTime::Cycles144,
            _ => SampleTime::Cycles480,
        }
    }

    pub fn us_for_cfg(&self, res: Resolution, sample_time: SampleTime) -> u32 {
        let res_clks = Self::get_res_clks(res);
        let sample_clks = Self::get_sample_time_clks(sample_time);
        (res_clks + sample_clks) * 1_000_000 / Self::freq().0
    }

    pub fn ns_for_cfg(&self, res: Resolution, sample_time: SampleTime) -> u64 {
        let res_clks = Self::get_res_clks(res);
        let sample_clks = Self::get_sample_time_clks(sample_time);
        (res_clks + sample_clks) as u64 * 1_000_000_000 / Self::freq().0 as u64
    }

    /// Perform a single conversion.
    fn convert(&mut self) -> u16 {
        // clear end of conversion flag
        T::regs().sr().modify(|reg| {
            reg.set_eoc(false);
        });

        // Start conversion
        T::regs().cr2().modify(|reg| {
            reg.set_adon(true);
            reg.set_swstart(true);
        });

        while T::regs().sr().read().strt() == false {
            // spin //wait for actual start
        }
        while T::regs().sr().read().eoc() == false {
            // spin //wait for finish
        }

        T::regs().dr().read().0 as u16
    }

    pub fn start_read_continuous(
        &mut self,
        rxdma: impl RxDma<T>,
        data: &mut [u16],
    ) -> Transfer<'static, impl dma::Channel> {
        use crate::dma::{Burst, FlowControl, TransferOptions};
        let rx_src = T::regs().dr().as_ptr() as *mut u16;
        let options = TransferOptions {
            pburst: Burst::Single,
            mburst: Burst::Single,
            flow_ctrl: FlowControl::Dma,
            fifo_threshold: None,
            circular: true,
            half_transfer_ir: false,
            complete_transfer_ir: true,
        };

        let req = rxdma.request();
        let transfer = unsafe { dma::Transfer::new_read_raw(rxdma, req, rx_src, data, options) };

        // while !transfer.is_running() {}

        //Enable ADC
        let was_on = Self::is_on();
        if !was_on {
            T::regs().cr2().modify(|reg| {
                reg.set_adon(false);
            });
        }

        T::regs().cr1().modify(|reg| {
            reg.set_scan(true);
            reg.set_discen(false);
            reg.set_eocie(true);
        });

        T::regs().cr2().modify(|reg| {
            reg.set_cont(vals::Cont::CONTINUOUS); //Goes with circular DMA
            reg.set_swstart(false);
            reg.set_dma(true);
            reg.set_dds(vals::Dds::CONTINUOUS);
            reg.set_eocs(vals::Eocs::EACHCONVERSION);
        });

        //Enable ADC
        T::regs().cr2().modify(|reg| {
            reg.set_adon(true);
            reg.set_swstart(true);
        });

        info!("ADC started");
        return transfer;
    }

    pub fn get_dma_buf<const N: usize>(
        &self,
        data: &mut [u16; N],
        transfer: &mut Transfer<'static, impl dma::Channel>,
    ) -> [u16; N] {
        // info!("Getting DMA buffer");
        // Stop DMA

        // info!("DMA stopped");
        // info!("Remaining transfers: {:?}", transfer.get_remaining_transfers());
        // Stop ADC conversions
        T::regs().cr2().modify(|reg| {
            reg.set_swstart(false);
            // reg.set_dma(false);
        });
        // info!(
        //     "{}/{}",
        //     transfer.get_complete_count(),
        //     transfer.get_remaining_transfers()
        // );

        //TODO: consider a little loop here checking the remaining transfers after stopping the adc.

        // info!("DMA requested stop");
        // Wait for DMA to stop

        // info!("Copying data {:#?}", data);
        let mut buf: [u16; N] = [0; N];
        // info!("Copying data2 {:#?}", data);
        buf.copy_from_slice(&data[..]);

        if !transfer.is_running() {
            //     transfer.reset_complete_count();
            transfer.request_restart();
        }
        //

        // return buf;
        // }
        // while !transfer.is_running() {}
        // info!("ADC stopped");

        // if transfer.get_remaining_transfers() < N as u16 {

        // buf.copy_from_slice(&mut data[..]);

        // transfer.request_restart();
        // Restart ADC conversions

        // transfer.request_restart();
        // transfer.reset_complete_count();
        T::regs().cr2().modify(|reg| {
            reg.set_adon(true);
            reg.set_swstart(true);
            // reg.set_dma(true);
        });

        // while !transfer.is_running() {
        //     info!("Waiting for DMA to restart");
        // }

        return buf;
    }

    pub async fn stop_continuous_conversion(&mut self) {
        T::regs().cr2().write(|reg| reg.set_adon(false));
        T::regs().cr2().modify(|reg| {
            reg.set_swstart(false);
            reg.set_dma(false);
        });
        T::regs().cr1().modify(|reg| {
            reg.set_eocie(false);
        });

        while T::regs().cr2().read().adon() {}
    }
}

impl<'d, T: Instance> Drop for Adc<'d, T> {
    fn drop(&mut self) {
        T::regs().cr2().modify(|reg| {
            reg.set_adon(false);
        });

        T::disable();
    }
}
