fn blah() {
    use esp_idf_svc::hal::gpio::{self, PinDriver};

    // Read from PIN 2
    let pin2 = unsafe {
        gpio::Gpio2::new()
    };

    // Note: Pure input pins can only be read from.
    // Pullup/pulldown cannot be set on pure input pins.
    let pin_driver = PinDriver::input(pin2)?;

    if pin_driver.is_high() {
       // Do something
    }
}

fn blah2() {
    use esp_idf_svc::hal::gpio::{self, PinDriver};

    // Read from PIN 2
    let pin2 = unsafe {
        gpio::Gpio2::new()
    };

    // Even if we only want to read from the pin,
    // we must make it an input/output pin to be able to set pull mode.
    let mut pin_driver = PinDriver::input_output(pin2)?;
    pin_driver.set_pull(gpio::Pull::Down)?;

    if pin_driver.is_high() {
       // Do something
    }
}

fn blah3() {
    use esp_idf_svc::hal::gpio::{self, PinDriver};

    let pin3 = unsafe { AnyIOPin::new(3) };
    let mut driver2 = PinDriver::output(pin3)?;
    let delay = esp_idf_svc::hal::delay::Delay::default();

    // Blink
    loop {
        driver2.set_high()?;
        delay.delay_ms(100);
        driver2.set_low()?;
        delay.delay_ms(100);
    }
}