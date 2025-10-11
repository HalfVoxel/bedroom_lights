#![deny(clippy::future_not_send)]
mod esp;
mod wifi;
mod wokwi;

use std::time::{Duration, Instant, SystemTime};

use brevduva::{channel::SerializationFormat, ReadWriteMode, SyncStorage};
use chrono::{DateTime, FixedOffset, TimeZone, Timelike, Utc};
use esp::init_esp;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver},
        prelude::*,
    },
    nvs::EspDefaultNvsPartition,
    ota::EspOta,
    sntp::EspSntp,
    sys::EspError,
    timer::EspTaskTimerService,
};
use log::{error, info, warn};
use smart_leds_trait::{SmartLedsWrite, White};
use wifi::start_wifi;
use wokwi::check_is_wokwi;
use ws2812_esp32_rmt_driver::RGBW8;
use ws2812_esp32_rmt_driver::{driver::color::LedPixelColorGrbw32, LedPixelEsp32Rmt};

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

struct DebugLed {
    driver: LedcDriver<'static>,
}

impl DebugLed {
    fn new(driver: LedcDriver<'static>) -> Self {
        Self { driver }
    }

    fn set_duty(&mut self, duty: f32) -> Result<(), EspError> {
        self.driver
            .set_duty((duty * self.driver.get_max_duty() as f32).round() as u32)?;
        Ok(())
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
}

fn successful_boot() {
    let mut ota = EspOta::new().expect("obtain OTA instance");
    ota.mark_running_slot_valid().expect("mark app as valid");
}

const SUNRISE_ANIMATION: &[(f32, [f32; 4])] = &[
    (0.0f32, [0.0, 0.0, 0.0, 0.0]),
    (1.0 * 60.0, [255.0, 70.0, 0.0, 0.0]),
    (3.0 * 60.0, [255.0, 87.0, 0.0, 87.0]),
    (5.0 * 60.0, [255.0, 123.0, 0.0, 123.0]),
    (20.0 * 60.0, [255.0, 123.0, 0.0, 160.0]),
    // (40.0, [255.0, 255.0, 255.0, 255.0]),
];

const PLANT_LIGHT: [f32; 4] = [255.0, 255.0, 255.0, 255.0];
const IN_BED_LIGHT: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
const EVENING_LIGHT: [f32; 4] = [255.0, 123.0, 0.0, 160.0];
const DITHER: [u32; 32] = [
    9, 3, 13, 7, 1, 10, 4, 14, 8, 2, 11, 5, 15, 9, 3, 13, 6, 0, 10, 4, 14, 7, 1, 11, 5, 15, 8, 2,
    12, 6, 0, 10,
];

#[derive(PartialEq, Eq, Debug, Clone, serde::Serialize, serde::Deserialize, Hash)]
struct InnerAlarmState {
    next_alarm: DateTime<Utc>,
    enabled: bool,
}

async fn async_main() -> Result<(), EspError> {
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let timer_service = EspTaskTimerService::new()?;

    let is_wokwi_simulator = check_is_wokwi()?;
    let mut debug_led = DebugLed::new(LedcDriver::new(
        peripherals.ledc.channel1,
        LedcTimerDriver::new(
            peripherals.ledc.timer1,
            &TimerConfig::default().frequency(5.kHz().into()),
        )?,
        peripherals.pins.gpio2,
    )?);
    debug_led.blink(4, Duration::from_millis(50)).await?;

    successful_boot();

    let mac = start_wifi(
        peripherals.modem,
        sys_loop.clone(),
        nvs,
        timer_service.clone(),
        is_wokwi_simulator,
    )
    .await;

    // convert mac to string
    let mac_str = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let ntp = EspSntp::new_default().unwrap();

    info!("Creating storage...");

    let device_id = format!("{MQTT_CLIENT_ID} {mac_str}");
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

    let lights = storage
        .add_container(
            &format!("lights/{device_id}/rgba"),
            Some([0, 0, 0, 0]),
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let (status_channel, _) = storage
        .add_channel::<String>(
            &format!("lights/{device_id}/status"),
            SerializationFormat::String,
        )
        .await
        .unwrap();

    info!("Light...");

    let led_pin = peripherals.pins.gpio12;
    let channel = peripherals.rmt.channel0;
    let mut ws2812 = LedPixelEsp32Rmt::<RGBW8, LedPixelColorGrbw32>::new(channel, led_pin).unwrap();

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
            if t >= at && t < bt {
                return lerp(ac, bc, (t - at) / (bt - at));
            }
        }
        SUNRISE_ANIMATION.last().unwrap().1
    }

    let mut last = Instant::now();
    let mut wakeup_start = None;
    let mut last_played_time = None;

    let mut current_color: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
    let fade_speed = 0.2;

    let mut last_color = [0, 0, 0, 0];

    // TODO: Figure out how to set the local timezone
    let tz = FixedOffset::east_opt(3600 * 2).unwrap();

    for it in 0.. {
        let t = Instant::now();
        let dt = t - last;
        last = t;

        let now = Utc::now().with_timezone(&tz);
        let is_evening = now.hour() >= 17;

        let mut target_color = if is_evening {
            EVENING_LIGHT
        } else {
            PLANT_LIGHT
        };
        if is_playing.get().unwrap() {
            if wakeup_start.is_none() {
                wakeup_start = Some(Instant::now());
                last_played_time = Some(Instant::now());
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

            let alarm_state_mutex = alarm_state.get();
            let alarm_state_v = alarm_state_mutex.as_ref().unwrap();
            let alarm_set_soon = alarm_state_v.enabled
                && alarm_state_v
                    .next_alarm
                    .signed_duration_since(&now)
                    .num_hours()
                    < 12;

            let is_night = now.hour() < 11 || now.hour() > 22;
            let alarm_played_recently = last_played_time
                .map(|v| v.elapsed() < Duration::from_secs(30 * 60))
                .unwrap_or(false);

            // Disable light:
            // - during nighttime
            // - when the user is in bed
            // - if an alarm is set to some time within the next few hours (likely that the user is in bed)
            // - if the alarm was finished relatively recently (make sure the user has enough time to get out of bed).
            if is_night || is_user_in_bed.get().unwrap() || alarm_set_soon || alarm_played_recently
            {
                target_color = IN_BED_LIGHT;
            }
        }

        current_color = lerp(current_color, target_color, dt.as_secs_f32() * fade_speed);

        // Make input colors roughly linear in perceived brightness
        let mut gamma = [
            ((current_color[0] / 255.0).powi(2) * 2048.0).round() as u32,
            ((current_color[1] / 255.0).powi(2) * 2048.0).round() as u32,
            ((current_color[2] / 255.0).powi(2) * 2048.0).round() as u32,
            ((current_color[3] / 255.0).powi(2) * 2048.0).round() as u32,
        ];

        if let Some(color) = lights.get().unwrap() {
            gamma = [color[0], color[1], color[2], color[3]];
        }

        if it % 100 == 0 {
            let status = format!("{:?} {:?} {:?}", current_color, target_color, gamma);
            println!("{}", status);
        }

        if it % 1000 == 0 {
            status_channel
                .send(format!("{} Target: {target_color:?}", now.to_rfc3339()))
                .await;
        }

        if gamma == last_color && it != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        last_color = gamma;

        let pixels = std::iter::repeat(gamma)
            .enumerate()
            .map(|(i, col)| {
                let r = DITHER[i % DITHER.len()];
                const SCALING: u32 = 2048 / 128;
                RGBW8::new_alpha(
                    ((col[0] + r) / SCALING) as u8,
                    ((col[1] + r) / SCALING) as u8,
                    ((col[2] + r) / SCALING) as u8,
                    White(((col[3] + r) / SCALING) as u8),
                )
            })
            .take(144);

        // let t_write = Instant::now();
        ws2812.write(pixels).unwrap();
        // println!("{:?} {:?}", dt, t_write.elapsed());
        // tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Ok(())
}
