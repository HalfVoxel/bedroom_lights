#![deny(clippy::future_not_send)]
mod esp;
mod wifi;
mod wokwi;

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brevduva::{channel::SerializationFormat, ReadWriteMode, SyncStorage};
use chrono::{DateTime, FixedOffset, Timelike, Utc};
use esp::init_esp;
use esp_idf_svc::hal::reset::ResetReason;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::timer::EspTimer;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        adc::{
            attenuation::DB_11,
            oneshot::{config::AdcChannelConfig, AdcChannelDriver, AdcDriver},
        },
        ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver},
        prelude::*,
    },
    nvs::EspDefaultNvsPartition,
    ota::EspOta,
    sntp::EspSntp,
    sys::EspError,
};
use log::{info, warn};
use smart_leds::RGB8;
use wifi::start_wifi;
use wokwi::check_is_wokwi;
// use ws2812_esp32_rmt_driver::driver::color::LedPixelColorGrb24;
// use ws2812_esp32_rmt_driver::LedPixelEsp32Rmt;

const MQTT_HOST: &str = "mqtt://arongranberg.com:1883";
const MQTT_CLIENT_ID: &str = "bedroom_lights";
const MQTT_USERNAME: &str = "wakeup_alarm";
const MQTT_PASSWORD: &str = "xafzz25nomehasff";

struct Logger {}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Trace
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            println!("{} - {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger {};

fn main() {
    init_esp();

    warn!(
        "ESP max log level={:?} log crate max level={:?}",
        esp_idf_svc::log::EspLogger::default().get_max_level(),
        log::STATIC_MAX_LEVEL
    );
    esp_idf_svc::log::set_target_level("*", log::LevelFilter::Info).unwrap();
    esp_idf_svc::log::set_target_level("brevduva", log::LevelFilter::Trace).unwrap();
    esp_idf_svc::log::set_target_level("bedroom_lights3", log::LevelFilter::Trace).unwrap();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
        .unwrap();
}

struct DebugLedDithered {
    desired_intensity: AtomicU32,
    current_intensity: AtomicU32,
    smoothing_factor: f32,
    resolution: u32,
}

impl DebugLedDithered {
    pub fn resolution(&self) -> u32 {
        self.resolution
    }

    fn set_intensity(&self, duty: f32) -> Result<(), EspError> {
        self.desired_intensity
            .store(duty.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    fn get_intensity(&self) -> f32 {
        f32::from_bits(self.desired_intensity.load(Ordering::Relaxed))
    }

    fn update_smoothing(&self, dt_secs: f32) -> f32 {
        let current = f32::from_bits(self.current_intensity.load(Ordering::Relaxed));
        let desired = f32::from_bits(self.desired_intensity.load(Ordering::Relaxed));
        let alpha = self.smoothing_factor * dt_secs;
        let new_intensity = current + (desired - current) * alpha.clamp(0.0, 1.0);
        self.current_intensity
            .store(new_intensity.to_bits(), Ordering::Relaxed);
        new_intensity
    }
}

struct DebugLed {
    driver: LedcDriver<'static>,
}

impl DebugLed {
    fn new(driver: LedcDriver<'static>) -> Self {
        Self { driver }
    }

    pub fn resolution(&self) -> u32 {
        self.driver.get_max_duty()
    }

    fn set_duty_raw(&mut self, duty: u32) -> Result<(), EspError> {
        self.driver.set_duty(duty)
    }

    fn set_duty(&mut self, duty: f32) -> Result<(), EspError> {
        self.set_duty_raw((duty * self.driver.get_max_duty() as f32).round() as u32)
    }

    fn set_duty_dithered(&mut self, duty: f32, time_index: usize) -> Result<(), EspError> {
        self.set_duty_raw(
            ((duty as f64) * self.driver.get_max_duty() as f64
            // (((time_index % 500) as f32 / 500.0)
                + (DITHER[time_index % DITHER.len()] as f64 * (1.0/16.0)))
                .floor() as u32,
        )
    }

    async fn blink(&mut self, times: usize, period: Duration) -> Result<(), EspError> {
        for _ in 0..times {
            self.set_duty(1.0)?;
            tokio::time::sleep(period).await;
            self.set_duty(0.0)?;
            tokio::time::sleep(period).await;
        }
        Ok(())
    }

    pub fn to_dithered(&self) -> DebugLedDithered {
        DebugLedDithered {
            desired_intensity: AtomicU32::new(0f32.to_bits()),
            current_intensity: AtomicU32::new(0f32.to_bits()),
            smoothing_factor: 0.2,
            resolution: self.resolution(),
        }
    }
}

fn successful_boot() {
    let mut ota = EspOta::new().expect("obtain OTA instance");
    ota.mark_running_slot_valid().expect("mark app as valid");
}

const SUNRISE_ANIMATION_RGBW: &[(f32, [f32; 4])] = &[
    (0.0f32, [0.0, 0.0, 0.0, 0.0]),
    (1.0 * 60.0, [255.0, 70.0, 0.0, 0.0]),
    (3.0 * 60.0, [255.0, 87.0, 0.0, 87.0]),
    (5.0 * 60.0, [255.0, 123.0, 0.0, 123.0]),
    (20.0 * 60.0, [255.0, 123.0, 0.0, 160.0]),
];

const SUNRISE_ANIMATION: &[(f32, [f32; 4])] = &[
    (0.0, [0.0, 0.0, 0.0, 0.0]),
    (1.0 * 60.0, [0.0, 0.0, 100.0, 0.0]),
    (3.0 * 60.0, [0.0, 50.0, 140.0, 0.0]),
    (5.0 * 60.0, [20.0, 100.0, 180.0, 0.0]),
    (20.0 * 60.0, [150.0, 100.0, 200.0, 0.0]),
];

const DITHER: [u32; 32] = [
    9, 3, 13, 7, 1, 10, 4, 14, 8, 2, 11, 5, 15, 9, 3, 13, 6, 0, 10, 4, 14, 7, 1, 11, 5, 15, 8, 2,
    12, 6, 0, 10,
];

#[derive(PartialEq, Eq, Debug, Clone, serde::Serialize, serde::Deserialize, Hash)]
struct InnerAlarmState {
    next_alarm: DateTime<Utc>,
    enabled: bool,
}

#[derive(PartialEq, Eq, Debug, Clone, serde::Serialize, serde::Deserialize, Hash)]
struct AlarmLastPlayed {
    last_played_time: Option<DateTime<Utc>>,
}

async fn blink_strips(power_levels: &mut [DebugLed]) -> Result<(), EspError> {
    let up_dur = Duration::from_millis(100);
    let down_dur = Duration::from_millis(200);
    let stagger_dur = Duration::from_millis(60);

    fn eval_local(t: f32, up: f32, down: f32) -> f32 {
        // t in seconds, up/down in seconds
        let r = if t <= 0.0 {
            0.0
        } else if t < up {
            // ease-in (quadratic)
            let x = t / up;
            x
            // (x * x).clamp(0.0, 1.0)
        } else if t < up + down {
            // ease-out (quadratic)
            let x = (t - up) / down;
            // (1.0 - x * x).clamp(0.0, 1.0)
            (1.0 - x).clamp(0.0, 1.0)
        } else {
            0.0
        };
        r * r
    }

    let n = power_levels.len();
    let total_secs = (stagger_dur.as_secs_f32() * (n.saturating_sub(1) as f32))
        + up_dur.as_secs_f32()
        + down_dur.as_secs_f32();

    let start = Instant::now();
    loop {
        let elapsed = start.elapsed().as_secs_f32();

        // stop once the whole staggered sequence finished
        if elapsed > total_secs {
            break;
        }

        for (i, pl) in power_levels.iter_mut().enumerate() {
            let offset = stagger_dur.as_secs_f32() * (i as f32);
            let local_t = elapsed - offset;
            let v = eval_local(local_t, up_dur.as_secs_f32(), down_dur.as_secs_f32());
            pl.set_duty(v * 0.1)?;
        }

        // update at ~10ms intervals to keep the ramps smooth
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // ensure all are off at the end
    for pl in power_levels.iter_mut() {
        pl.set_duty(0.0)?;
    }

    tokio::time::sleep(Duration::from_millis(1000)).await;

    for p in 0..10 {
        for pl in power_levels.iter_mut() {
            pl.set_duty_raw(p)?;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    for p in 0..10 {
        for pl in power_levels.iter_mut() {
            pl.set_duty_raw(9 - p)?;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    let t0 = Instant::now();
    for it in 0.. {
        let elapsed = t0.elapsed().as_secs_f32();
        if elapsed > 20.0 {
            break;
        }
        let v = if elapsed < 10.0 {
            (elapsed / 10.0) * 9.0
        } else {
            ((20.0 - elapsed) / 10.0) * 9.0
        };
        for pl in power_levels.iter_mut() {
            pl.set_duty_dithered(v / (pl.resolution() as f32), it)?;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    Ok(())
}

async fn blink_strips_d(power_levels: &[DebugLedDithered]) -> Result<(), EspError> {
    let up_dur = Duration::from_millis(100);
    let down_dur = Duration::from_millis(200);
    let stagger_dur = Duration::from_millis(60);

    fn eval_local(t: f32, up: f32, down: f32) -> f32 {
        // t in seconds, up/down in seconds
        let r = if t <= 0.0 {
            0.0
        } else if t < up {
            // ease-in (quadratic)
            let x = t / up;
            x
            // (x * x).clamp(0.0, 1.0)
        } else if t < up + down {
            // ease-out (quadratic)
            let x = (t - up) / down;
            // (1.0 - x * x).clamp(0.0, 1.0)
            (1.0 - x).clamp(0.0, 1.0)
        } else {
            0.0
        };
        r * r
    }

    let n = power_levels.len();
    let total_secs = (stagger_dur.as_secs_f32() * (n.saturating_sub(1) as f32))
        + up_dur.as_secs_f32()
        + down_dur.as_secs_f32();

    let start = Instant::now();
    loop {
        let elapsed = start.elapsed().as_secs_f32();

        // stop once the whole staggered sequence finished
        if elapsed > total_secs {
            break;
        }

        for (i, pl) in power_levels.iter().enumerate() {
            let offset = stagger_dur.as_secs_f32() * (i as f32);
            let local_t = elapsed - offset;
            let v = eval_local(local_t, up_dur.as_secs_f32(), down_dur.as_secs_f32());
            pl.set_intensity(v * 0.05)?;
        }

        // update at ~10ms intervals to keep the ramps smooth
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // ensure all are off at the end
    for pl in power_levels.iter() {
        pl.set_intensity(0.0)?;
    }

    tokio::time::sleep(Duration::from_millis(1000)).await;

    let t0 = Instant::now();
    for it in 0.. {
        let elapsed = t0.elapsed().as_secs_f32();
        if elapsed > 20.0 {
            break;
        }
        let v = if elapsed < 10.0 {
            (elapsed / 10.0) * 9.0
        } else {
            ((20.0 - elapsed) / 10.0) * 9.0
        };
        for pl in power_levels.iter() {
            pl.set_intensity(v / (pl.resolution() as f32))?;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Ok(())
}

fn lerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let mut res = [0.0; 4];
    for i in 0..4 {
        res[i] = (a[i] + (b[i] - a[i]) * t).clamp(0.0, 255.0);
    }
    res
}

fn get_wakup_color(t: f32) -> [f32; 4] {
    for i in 0..SUNRISE_ANIMATION.len() - 1 {
        let (at, ac) = SUNRISE_ANIMATION[i];
        let (bt, bc) = SUNRISE_ANIMATION[i + 1];
        assert!(
            bt > at,
            "Animation keyframes must be in increasing time order"
        );
        if t >= at && t < bt {
            return lerp(ac, bc, (t - at) / (bt - at));
        }
    }
    SUNRISE_ANIMATION.last().unwrap().1
}

#[test]
fn test_lerp() {
    let a = [0.0, 0.0, 0.0, 0.0];
    let b = [100.0, 200.0, 300.0, 400.0];
    let r = lerp(a, b, 0.5);
    assert_eq!(r, [50.0, 100.0, 150.0, 200.0]);
    assert_eq!(lerp(a, b, 0.0), a);
    assert_eq!(lerp(a, b, 1.0), b);
}

#[derive(PartialEq, Eq, Clone, Hash, Copy)]
struct RGBColor {
    r: u8,
    g: u8,
    b: u8,
}

impl std::fmt::Debug for RGBColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({},{},{})", self.r, self.g, self.b)
    }
}

impl serde::Serialize for RGBColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let rgb = format!("rgb({},{},{})", self.r, self.g, self.b);
        serializer.serialize_str(&rgb)
    }
}

impl From<RGBColor> for [f32; 4] {
    fn from(color: RGBColor) -> Self {
        [color.r as f32, color.g as f32, color.b as f32, 0.0]
    }
}

impl<'de> serde::Deserialize<'de> for RGBColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let rgb: &str = serde::Deserialize::deserialize(deserializer)?;
        if let Some(stripped) = rgb.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
            let parts: Vec<&str> = stripped.split(',').map(|s| s.trim()).collect();
            if parts.len() == 3 {
                let r = parts[0].parse::<u8>().map_err(serde::de::Error::custom)?;
                let g = parts[1].parse::<u8>().map_err(serde::de::Error::custom)?;
                let b = parts[2].parse::<u8>().map_err(serde::de::Error::custom)?;
                return Ok(RGBColor { r, g, b });
            }
        }
        Err(serde::de::Error::custom("Invalid rgb(r,g,b) color format"))
    }
}
async fn async_main() -> Result<(), EspError> {
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let timer_service = esp_idf_svc::timer::EspTaskTimerService::new()?;

    let is_wokwi_simulator = check_is_wokwi()?;

    let driver = LedcTimerDriver::new(
        peripherals.ledc.timer2,
        &TimerConfig::default()
            // We want a high resolution to be able to smoothly dim the lights down to very low levels.
            // The frequency needs to be kept reasonably high to avoid visible flickering.
            // See https://gist.github.com/benpeoples/3aa57bffc0f26ede6623ca520f26628c
            .frequency(610.Hz().into())
            .resolution(esp_idf_svc::hal::ledc::Resolution::Bits17),
    )?;

    let mut debug_led = DebugLed::new(LedcDriver::new(
        peripherals.ledc.channel1,
        &driver,
        peripherals.pins.gpio2,
    )?);
    debug_led.blink(4, Duration::from_millis(50)).await?;

    // Configure GPIO34 as an ADC input (potentiometer)
    let adc = AdcDriver::new(peripherals.adc1)?;

    let mut adc_pin = AdcChannelDriver::new(
        &adc,
        peripherals.pins.gpio34,
        &AdcChannelConfig {
            attenuation: DB_11,
            ..Default::default()
        },
    )?;

    let mut power_levels = [
        DebugLed::new(LedcDriver::new(
            peripherals.ledc.channel2,
            &driver,
            peripherals.pins.gpio33,
        )?),
        DebugLed::new(LedcDriver::new(
            peripherals.ledc.channel3,
            &driver,
            peripherals.pins.gpio25,
        )?),
        DebugLed::new(LedcDriver::new(
            peripherals.ledc.channel4,
            &driver,
            peripherals.pins.gpio26,
        )?),
    ];

    // Prepare to move the LED drivers into a short-lock critical section
    // and expose lock-free desired setpoints. A timer callback running at
    // ~100Hz will perform the dithered writes from a tight (interrupt-like)
    // context so other async work doesn't interfere with the timing.

    // Keep the resolution for use later (power_levels is moved below).
    info!("Dimmer has resolution {}", power_levels[0].resolution());
    let min_resolution_step = 0.1 / (power_levels[0].resolution() as f32);

    // blink_strips(&mut power_levels).await?;

    // Desired levels stored as f32 bit-patterns in AtomicU32 for lock-free writes.
    let desired: Arc<Vec<DebugLedDithered>> =
        Arc::new(power_levels.iter().map(DebugLed::to_dithered).collect());

    // Small counter used to index the dither table.
    let dither_index = Arc::new(AtomicUsize::new(0));
    let last_dt = Arc::new(AtomicU32::new(0f32.to_bits()));

    // Schedule a periodic callback at ~100Hz (10ms). The EspTaskTimerService
    // callback executes in a timer/dispatch context; keep the body minimal.
    let timer = {
        let desired = desired.clone();
        let dither_index = dither_index.clone();
        let mut last_t = Instant::now();
        let last_dt = last_dt.clone();
        timer_service.timer(move || {
            let now_t = Instant::now();
            let dt_secs = (now_t - last_t).as_secs_f32();
            last_t = now_t;
            last_dt.store((dt_secs * 1000.0).to_bits(), Ordering::Relaxed);
            let idx = dither_index.fetch_add(1, Ordering::Relaxed);
            // Iterate quickly and perform dithered writes. Ignore errors.
            for (led, desired) in power_levels.iter_mut().zip(desired.iter()) {
                let intensity = desired.update_smoothing(dt_secs);
                let gamma = intensity * intensity; // simple gamma correction
                let _ = led.set_duty_dithered(gamma, idx);
            }
        })?
    };
    timer.every(Duration::from_millis(2))?;

    let (mac, blink_res) = tokio::join!(
        start_wifi(
            peripherals.modem,
            sys_loop.clone(),
            nvs,
            timer_service.clone(),
            is_wokwi_simulator,
        ),
        blink_strips_d(&desired),
    );
    blink_res?;

    // convert mac to string
    let mac_str = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let ntp = EspSntp::new_default().unwrap();

    info!("Creating storage...");

    let device_id = format!("{MQTT_CLIENT_ID} {mac_str}");
    info!("Device ID: {}", device_id);
    let storage = SyncStorage::new(
        &device_id,
        MQTT_HOST,
        MQTT_USERNAME,
        MQTT_PASSWORD,
        brevduva::SessionPersistance::Persistent,
    )
    .await;

    info!("Containers...");

    ota_flasher::downloader::initialize_ota(&storage, &device_id, env!("BUILD_ID")).await;

    successful_boot();

    let (status_channel, _) = storage
        .add_channel::<String>(
            &format!("lights/{device_id}/status"),
            SerializationFormat::String,
        )
        .await
        .unwrap();

    match ResetReason::get() {
        ResetReason::Panic | ResetReason::TaskWatchdog | ResetReason::CPULockup => {
            debug_led
                .blink(3, Duration::from_millis(200))
                .await
                .unwrap();
            status_channel
                .send("Restart was due to panic. Entering safe mode for 30 seconds.".to_string())
                .await;
            // Sleep
            debug_led
                .blink(30, Duration::from_millis(500))
                .await
                .unwrap();
            status_channel
                .send("Exiting safe mode after panic.".to_string())
                .await;
        }
        _ => { /* all good */ }
    }

    let alarm_state = storage
        .add_container_with_mode(
            "alarm/state",
            InnerAlarmState {
                next_alarm: Default::default(),
                enabled: false,
            },
            SerializationFormat::Auto,
            ReadWriteMode::ReadOnly,
        )
        .await
        .unwrap();
    let is_user_in_bed = storage
        .add_container_with_mode(
            "alarm/+/is_user_in_bed",
            false,
            SerializationFormat::Auto,
            ReadWriteMode::ReadOnly,
        )
        .await
        .unwrap();
    let is_playing = storage
        .add_container_with_mode(
            "alarm/+/is_playing",
            false,
            SerializationFormat::Auto,
            ReadWriteMode::ReadOnly,
        )
        .await
        .unwrap();
    let alarm_last_played = storage
        .add_container_with_mode(
            "alarm/last_played",
            AlarmLastPlayed {
                last_played_time: None,
            },
            SerializationFormat::Auto,
            ReadWriteMode::ReadOnly,
        )
        .await
        .unwrap();

    let lights = storage
        .add_container::<Option<[u32; 4]>>(
            &format!("lights/{device_id}/rgba"),
            None,
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let lights_actual = storage
        .add_container::<Option<[u32; 4]>>(
            &format!("lights/{device_id}/rgba_actual"),
            None,
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    // const PLANT_LIGHT: [f32; 4] = [255.0, 255.0, 60.0, 0.0];
    // const IN_BED_LIGHT: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
    // const EVENING_LIGHT: [f32; 4] = [20.0, 128.0, 160.0, 0.0];
    // const SNOOZE_LIGHT: [f32; 4] = [0.0, 0.0, 60.0, 0.0];

    let snooze_light_color = storage
        .add_container::<RGBColor>(
            &format!("lights/colors/snooze"),
            RGBColor { r: 0, g: 0, b: 60 },
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let plant_light_color = storage
        .add_container::<RGBColor>(
            &format!("lights/colors/plant"),
            RGBColor {
                r: 255,
                g: 255,
                b: 60,
            },
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let evening_light_color = storage
        .add_container::<RGBColor>(
            &format!("lights/colors/evening"),
            RGBColor {
                r: 20,
                g: 128,
                b: 160,
            },
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let in_bed_light_color = storage
        .add_container::<RGBColor>(
            &format!("lights/colors/in_bed"),
            RGBColor { r: 0, g: 0, b: 0 },
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    info!("Light...");

    let led_pin = peripherals.pins.gpio32; // TODO: 33
    let channel = peripherals.rmt.channel0;
    // let mut ws2812 = LedPixelEsp32Rmt::<RGB8, LedPixelColorGrb24>::new(channel, led_pin).unwrap();

    info!("Waiting for sync...");

    debug_led.blink(1, Duration::from_millis(100)).await?;

    // Wait until we have current time from network
    while ntp.get_sync_status() != esp_idf_svc::sntp::SyncStatus::Completed {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    debug_led.blink(2, Duration::from_millis(40)).await?;

    storage.wait_for_sync().await;

    debug_led.blink(10, Duration::from_millis(20)).await?;

    status_channel.send(format!("Starting...")).await;

    info!("Loop...");

    let mut last = Instant::now();
    let mut wakeup_start = None;
    let mut last_played_time = None;
    let mut last_played_trigger_time = None;

    let mut current_color: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
    let fade_speed = 0.2;

    let mut last_color = [0.0, 0.0, 0.0, 0.0];
    let mut target_color: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

    // TODO: Figure out how to set the local timezone
    let tz = FixedOffset::east_opt(3600 * 1).unwrap();

    status_channel.send(format!("Started")).await;

    for it in 0.. {
        let t = Instant::now();
        let dt = t - last;
        last = t;

        {
            let now = Utc::now().with_timezone(&tz);
            let is_evening = now.hour() >= 17;
            let is_morning = now.hour() < 11;

            target_color = if is_evening {
                evening_light_color.get().unwrap().into()
            } else {
                plant_light_color.get().unwrap().into()
            };

            let alarm_state_mutex = alarm_state.get();
            let alarm_state_v = alarm_state_mutex.as_ref().unwrap();
            let time_until_next_alarm = alarm_state_v.next_alarm.signed_duration_since(&now);

            let alarm_set_very_soon =
                alarm_state_v.enabled && time_until_next_alarm.num_seconds() < 30; // Start wakeup light a little while before alarm
            let alarm_has_passed = time_until_next_alarm.num_seconds() < -60; // Allow for some leeway in clock sync between devices
            if is_playing.get().unwrap() || (alarm_set_very_soon && !alarm_has_passed) {
                if wakeup_start.is_none() {
                    wakeup_start = Some(Instant::now());
                    last_played_time = Some(Instant::now());
                    last_played_trigger_time = Some(alarm_state_v.next_alarm.clone());
                    status_channel
                        .send("Detected alarm is playing".to_string())
                        .await;
                }
                target_color = get_wakup_color(wakeup_start.unwrap().elapsed().as_secs_f32());
            } else {
                if wakeup_start.is_some() {
                    wakeup_start = None;
                    status_channel
                        .send("Detected alarm stopped playing".to_string())
                        .await;
                }

                let alarm_set_soon = alarm_state_v.enabled
                    && time_until_next_alarm.num_hours() < 12
                    && time_until_next_alarm.num_seconds() >= 0;

                let alarm_last_played_time = alarm_last_played.get().clone().unwrap();
                let alarm_played_recently = alarm_last_played_time
                    .last_played_time
                    .map(|v| now.signed_duration_since(v).num_minutes() < 30)
                    .unwrap_or(false);

                let is_night = now.hour() < 11 || now.hour() > 22;

                if alarm_played_recently
                    && alarm_state_v.enabled
                    && last_played_trigger_time == Some(alarm_state_v.next_alarm)
                {
                    // When the alarm was played recently and is still enabled with the same trigger time,
                    // assume the user has snoozed.
                    target_color = snooze_light_color.get().unwrap().into();
                } else if is_night || alarm_set_soon || alarm_played_recently {
                    // Disable light:
                    // - during nighttime
                    // - when the user is in bed
                    // - if an alarm is set to some time within the next few hours (likely that the user is in bed)
                    // - if the alarm was finished relatively recently (make sure the user has enough time to get out of bed).
                    target_color = in_bed_light_color.get().unwrap().into();
                } else if is_user_in_bed.get().unwrap() {
                    if is_evening || is_morning {
                        target_color = in_bed_light_color.get().unwrap().into();
                    } else {
                        // If the user is in bed during daytime, set a soft light to
                        // avoid complete darkness.
                        target_color = evening_light_color.get().unwrap().into();
                    }
                }
            }
        }

        // current_color = lerp(current_color, target_color, dt.as_secs_f32() * fade_speed);

        let mut gamma = [
            (target_color[0] / 255.0),
            (target_color[1] / 255.0),
            (target_color[2] / 255.0),
            (target_color[3] / 255.0),
        ];

        if let Some(color) = lights.get().unwrap() {
            gamma = [
                color[0] as f32 / 100.0,
                color[1] as f32 / 100.0,
                color[2] as f32 / 100.0,
                color[3] as f32 / 100.0,
            ];
        }

        // if it % 100 == 0 {
        //     let status = format!("{:?} {:?} {:?}", current_color, target_color, gamma);
        //     println!("{}", status);
        // }

        // gamma[0] = (adc_pin.read_raw()? as f32 / 4095.0).powi(3);

        // if gamma
        //     .iter()
        //     .zip(&last_color)
        //     .all(|(&g, &l)| (g - l).abs() < min_resolution_step)
        //     && it != 0
        // {
        //     // If there's no significant color change, go to low power mode
        //     // after a while to save power.
        //     // Wait for a few iterations to run dithering a bit before stabilizing,
        //     // and to avoid accidentally entering low power during smooth transitions.
        //     low_power_counter += 1;
        //     if low_power_counter > 50 {
        //         if !low_power {
        //             status_channel.send(format!("Entering low power")).await;
        //             low_power = true;
        //         }

        //         tokio::time::sleep(Duration::from_millis(10)).await;
        //         continue;
        //     }
        // } else {
        //     low_power = false;
        //     low_power_counter = 0;
        // }
        // last_color = gamma;

        desired[0].set_intensity(gamma[0])?;
        desired[1].set_intensity(gamma[1])?;
        desired[2].set_intensity(gamma[2])?;

        // let pixels = std::iter::repeat(gamma)
        //     .enumerate()
        //     .map(|(i, col)| {
        //         let r = DITHER[i % DITHER.len()] & 0x3;
        //         const SCALING: u32 = 2048 / 256;
        //         // RGBW8::new_alpha(
        //         //     ((col[0] + r) / SCALING) as u8,
        //         //     ((col[1] + r) / SCALING) as u8,
        //         //     ((col[2] + r) / SCALING) as u8,
        //         //     White(((col[3] + r) / SCALING) as u8),
        //         // )
        //         RGB8::new(
        //             ((col[0] + r) / SCALING).min(255) as u8,
        //             ((col[1] + r) / SCALING).min(255) as u8,
        //             ((col[2] + r) / SCALING).min(255) as u8,
        //         )
        //     })
        //     .take(60);

        // // let t_write = Instant::now();
        // ws2812.write(pixels).unwrap();
        // println!("{:?} {:?}", dt, t_write.elapsed());
        // tokio::task::yield_now().await;

        if it % 20 == 0 {
            lights_actual
                .set(Some([
                    (gamma[0] * 100.0) as u32,
                    (gamma[1] * 100.0) as u32,
                    (gamma[2] * 100.0) as u32,
                    (gamma[3] * 100.0) as u32,
                ]))
                .await;

            // let last_dt = f32::from_bits(last_dt.load(Ordering::Relaxed));
            // status_channel
            //     .send(format!("Target: {gamma:.3?} {dt:.3?} {last_dt:.3?}"))
            //     .await;
            // status_channel.send(format!("{last_dt:.3?}")).await;
            // info!("{last_dt:.3?}");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}
