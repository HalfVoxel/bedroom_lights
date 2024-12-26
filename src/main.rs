#![deny(clippy::future_not_send)]
mod esp;
mod wifi;
mod wokwi;

use std::time::{Duration, Instant};

use brevduva::SyncStorage;
use esp::init_esp;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver},
        prelude::*,
        task::current,
    },
    nvs::EspDefaultNvsPartition,
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

fn main() -> anyhow::Result<()> {
    // env_logger::builder()
    //     .filter_level(log::LevelFilter::Trace)
    //     .init();
    esp_idf_svc::log::set_target_level("brevduva", log::LevelFilter::Trace).unwrap();
    init_esp();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
        .unwrap();

    info!("Shutting down in 5s...");

    std::thread::sleep(core::time::Duration::from_secs(5));

    Ok(())
}

struct DebugLed {
    driver: LedcDriver<'static>,
}

impl DebugLed {
    fn new(driver: LedcDriver<'static>) -> Self {
        Self { driver }
    }

    fn set_duty(&mut self, duty: f32) -> anyhow::Result<()> {
        self.driver
            .set_duty((duty * self.driver.get_max_duty() as f32).round() as u32)?;
        Ok(())
    }

    async fn blink(&mut self, times: usize, period: Duration) -> anyhow::Result<()> {
        for _ in 0..times {
            self.set_duty(1.0)?;
            tokio::time::sleep(period).await;
            self.set_duty(0.0)?;
            tokio::time::sleep(period).await;
        }
        Ok(())
    }
}

async fn async_main() -> anyhow::Result<()> {
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
    debug_led.blink(5, Duration::from_millis(50)).await?;

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

    info!("Creating storage...");

    let device_id = format!("{MQTT_CLIENT_ID} {mac_str}");
    let storage = SyncStorage::new(&device_id, MQTT_HOST, MQTT_USERNAME, MQTT_PASSWORD).await;

    info!("Containers...");

    let is_user_in_bed = storage
        .add_container("alarm/+/is_user_in_bed", false)
        .await
        .unwrap();
    let is_playing = storage
        .add_container("alarm/+/is_playing", false)
        .await
        .unwrap();

    let lights = storage
        .add_container(&format!("lights/{device_id}/rgba"), Some([0, 0, 0, 0]))
        .await
        .unwrap();

    let reboot = storage
        .add_container("lights/+/reboot", false)
        .await
        .unwrap();

    info!("Light...");

    let led_pin = peripherals.pins.gpio12;
    let channel = peripherals.rmt.channel0;
    let mut ws2812 = LedPixelEsp32Rmt::<RGBW8, LedPixelColorGrbw32>::new(channel, led_pin).unwrap();

    info!("Waiting for sync...");

    debug_led.blink(1, Duration::from_millis(100)).await?;

    storage.wait_for_sync().await;

    debug_led.blink(10, Duration::from_millis(20)).await?;

    reboot.set(false).await;

    info!("Loop...");

    // [255,20,0,0]
    // [255,30,0,30]
    // [255,60,0,60]
    // On: [255,255,255,255]

    let animations = &[
        (0.0f32, [0.0, 0.0, 0.0, 0.0]),
        (1.0 * 60.0, [255.0, 70.0, 0.0, 0.0]),
        (3.0 * 60.0, [255.0, 87.0, 0.0, 87.0]),
        (5.0 * 60.0, [255.0, 123.0, 0.0, 123.0]),
        (20.0 * 60.0, [255.0, 123.0, 0.0, 160.0]),
        // (40.0, [255.0, 255.0, 255.0, 255.0]),
    ];

    fn lerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
        let mut res = [0.0; 4];
        for i in 0..4 {
            res[i] = (a[i] + (b[i] - a[i]) * t).clamp(0.0, 255.0);
        }
        res
    }

    let get_wakup_color = move |t: f32| {
        for i in 0..animations.len() - 1 {
            let (at, ac) = animations[i];
            let (bt, bc) = animations[i + 1];
            if t >= at && t < bt {
                return lerp(ac, bc, (t - at) / (bt - at));
            }
        }
        animations.last().unwrap().1
    };

    let mut last = Instant::now();
    let mut wakeup_start = None;

    let plant_light = [255.0, 255.0, 255.0, 255.0];
    let in_bed_light = [0.0, 0.0, 0.0, 0.0];
    let mut current_color: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
    let fade_speed = 0.2;

    let dither: &[u32; 32] = &[
        9, 3, 13, 7, 1, 10, 4, 14, 8, 2, 11, 5, 15, 9, 3, 13, 6, 0, 10, 4, 14, 7, 1, 11, 5, 15, 8,
        2, 12, 6, 0, 10,
    ];

    for it in 0.. {
        let t = Instant::now();
        let dt = t - last;
        last = t;

        let mut target_color = plant_light;
        if is_playing.get().unwrap() {
            if wakeup_start.is_none() {
                wakeup_start = Some(Instant::now());
            }
            target_color = get_wakup_color(wakeup_start.unwrap().elapsed().as_secs_f32());
        } else {
            wakeup_start = None;

            if is_user_in_bed.get().unwrap() {
                target_color = in_bed_light;
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
            println!(
                "{:?} {:?} {:?}",
                current_color,
                target_color, // dt.as_secs_f32() * fade_speed
                gamma,
            );
            // debug_led.blink(1, Duration::from_millis(10)).await?;
            // debug_led.set_duty(gamma[0] as f32 / 2048.0).unwrap();
        }

        let pixels = std::iter::repeat(gamma)
            .enumerate()
            .map(|(i, col)| {
                let r = dither[i % dither.len()];
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

    // let mut previous = [0, 0, 0, 0];
    // let mut current;
    // for (duration, col) in animations {
    //     let dur = Duration::from_secs(*duration);

    //     loop {
    //         let t = start.elapsed();
    //         if t > dur {
    //             break;
    //         }
    //         current = lerp(previous, *col, t.as_secs_f32() / dur.as_secs_f32());

    //         let pixel_col = RGBW8::new_alpha(current[0], current[1], current[2], White(current[3]));
    //         let pixels = std::iter::repeat(pixel_col).take(144 + 60);
    //         ws2812.write(pixels).unwrap();
    //         tokio::time::sleep(Duration::from_millis(20)).await;
    //     }

    //     previous = *col;
    // }

    info!("Up to date. Starting to update...");

    let pin = peripherals.pins.gpio17;
    let timer_driver = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::default().frequency(5.kHz().into()),
    )
    .unwrap();
    let mut driver = LedcDriver::new(peripherals.ledc.channel0, timer_driver, pin)?;
    driver.set_duty(0)?;

    let max_duty = driver.get_max_duty();
    println!("Max duty: {}", max_duty);

    let mut light = 0.0;
    let mut last_time = Instant::now();
    loop {
        let t = Instant::now();
        let dt = t - last_time;
        last_time = t;

        let is_user_in_bed = is_user_in_bed.get().unwrap();
        let is_playing = is_playing.get().unwrap();

        let (desired, speed) = match (is_user_in_bed, is_playing) {
            (true, true) => (0.8, 0.1),
            (true, false) => (0.0, 0.4),
            (false, _) => (1.0, 0.1),
        };

        light = light + (desired - light) * speed * dt.as_secs_f32();
        println!("Light: {light}. {is_user_in_bed}, {is_playing}");

        driver.set_duty(((max_duty as f32) * light) as u32)?;

        // Sleep using tokio
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}
