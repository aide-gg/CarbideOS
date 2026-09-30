// SPDX-License-Identifier: AGPL-3.0-or-later
//! Renders every installer screen to a PNG without a display.
//!
//! The installer only ever runs on bare metal against a real DRM device, so
//! reviewing a design change would otherwise mean a build and a reboot. This
//! drives the same software renderer the installer uses into a plain memory
//! buffer, with a clock we control so animations can be captured mid-flight
//! or fully settled.

use std::cell::RefCell;
use std::fs::File;
use std::io::BufWriter;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ModelRc, PhysicalSize, Rgb8Pixel, VecModel};

slint::include_modules!();

#[path = "../sample.rs"]
mod sample;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

thread_local! {
    static CLOCK: RefCell<Duration> = const { RefCell::new(Duration::ZERO) };
}

struct Preview {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for Preview {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        CLOCK.with(|c| *c.borrow())
    }
}

/// Advances the animation clock in steps. Slint samples time once per frame,
/// so stepping rather than jumping lets easing curves resolve the way they
/// would on hardware.
fn advance(window: &MinimalSoftwareWindow, buffer: &mut [Rgb8Pixel], by: Duration) {
    let mut now = CLOCK.with(|c| *c.borrow());
    let to = now + by;
    while now < to {
        now = (now + Duration::from_millis(16)).min(to);
        CLOCK.with(|c| *c.borrow_mut() = now);
        slint::platform::update_timers_and_animations();
        window.draw_if_needed(|renderer| {
            renderer.render(buffer, WIDTH as usize);
        });
    }
}

fn save(name: &str, buffer: &[Rgb8Pixel]) {
    let path = format!("preview/{name}.png");
    let file = File::create(&path).expect("create png");
    let mut encoder = png::Encoder::new(BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);

    let mut flat = Vec::with_capacity(buffer.len() * 3);
    for px in buffer {
        flat.extend_from_slice(&[px.r, px.g, px.b]);
    }
    encoder
        .write_header()
        .expect("png header")
        .write_image_data(&flat)
        .expect("png data");
    eprintln!("wrote {path}");
}

fn main() {
    std::fs::create_dir_all("preview").expect("preview directory");

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Preview {
        window: window.clone(),
    }))
    .expect("set platform");
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));

    let mut buffer = vec![Rgb8Pixel { r: 0, g: 0, b: 0 }; (WIDTH * HEIGHT) as usize];

    let new_ui = || {
        let ui = AppWindow::new().expect("build ui");
        ui.set_version("0.3.5".into());
        ui.set_disks(ModelRc::from(Rc::new(VecModel::from(sample::disks()))));
        ui.set_scan_status("3 storage devices ready".into());
        ui.set_image_status("PREPARING IMAGE  64%".into());
        ui.set_edition("Fleet".into());
        ui
    };

    // Each first-screen variant in a fresh window, so it boots from black as
    // on hardware. Caught resting on the boot mark, then settled.
    for (variant, edition, name) in [
        (0, "Fleet", "1a-editorial"),
        (1, "Fleet", "1b-minimal"),
        (2, "Fleet", "1c-backdrop"),
        (2, "", "1d-backdrop-dev"),
    ] {
        let ui = new_ui();
        ui.set_attract_variant(variant);
        ui.set_edition(edition.into());
        ui.show().expect("show");
        // One frame out, then reported as presented, as the installer does.
        advance(&window, &mut buffer, Duration::from_millis(16));
        ui.set_presented(true);
        advance(&window, &mut buffer, Duration::from_millis(1484));
        save(&format!("{name}-boot"), &buffer);
        advance(&window, &mut buffer, Duration::from_millis(2600));
        save(name, &buffer);
        ui.hide().expect("hide");
    }

    let ui = new_ui();
    ui.show().expect("show");

    ui.set_phase(Phase::Target);
    ui.set_selected(0);
    // What the operator normally sees: decoding finished behind the attract
    // screen well before they got here.
    ui.set_image_status("IMAGE READY".into());
    advance(&window, &mut buffer, Duration::from_millis(1200));
    save("2-target", &buffer);

    ui.set_phase(Phase::Confirm);
    advance(&window, &mut buffer, Duration::from_millis(800));
    save("3-confirm", &buffer);

    // Half of the confirmation code typed.
    ui.set_typed(2);
    advance(&window, &mut buffer, Duration::from_millis(500));
    save("4-typing", &buffer);
    ui.set_typed(0);

    ui.set_phase(Phase::Install);
    ui.set_progress(0.62);
    ui.set_stage("WRITING".into());
    ui.set_detail("1.84 GB/s".into());
    ui.set_summary_target("/dev/nvme0n1".into());
    advance(&window, &mut buffer, Duration::from_millis(500));
    save("5-install", &buffer);

    ui.set_phase(Phase::Summary);
    ui.set_summary_headline("Installation complete".into());
    ui.set_summary_elapsed("2.31 s".into());
    ui.set_summary_written("1.18 GiB".into());
    ui.set_summary_average("548 MB/s".into());
    ui.set_summary_peak("1.92 GB/s".into());
    ui.set_summary_target("/dev/nvme0n1  Samsung SSD 990 PRO 2TB".into());
    ui.set_summary_image("carbideos 0.3.5".into());
    advance(&window, &mut buffer, Duration::from_millis(1400));
    save("6-summary", &buffer);
}
