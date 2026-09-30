// SPDX-License-Identifier: AGPL-3.0-or-later
//! Records installer scenes as raw RGB24 at 60 fps on stdout. Every frame is
//! rendered on a controlled clock, so the output is exact regardless of how
//! fast this machine is. Frames are double-buffered with partial repaints, as
//! on a DRM dumb-buffer display, so redraw bugs show up here too.
//!
//! cargo run --features preview --bin carbide-record -- SCENE | \
//!     ffmpeg -f rawvideo -pix_fmt rgb24 -s 1920x1080 -r 60 -i - out.mp4

use std::cell::RefCell;
use std::io::{BufWriter, Write};
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{ModelRc, PhysicalSize, Rgb8Pixel, SharedString, VecModel};

slint::include_modules!();

#[path = "../sample.rs"]
mod sample;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const FPS: u64 = 60;
const SCENES: &str = "boot, confirm-hold, confirm-type, confirm-countdown or flow";
const BLACK: Rgb8Pixel = Rgb8Pixel { r: 0, g: 0, b: 0 };

thread_local! {
    static CLOCK: RefCell<Duration> = const { RefCell::new(Duration::ZERO) };
}

struct Recorder {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for Recorder {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        CLOCK.with(|c| *c.borrow())
    }
}

struct Tape<W: Write> {
    out: W,
    frame: u64,
    bytes: Vec<u8>,
    window: Rc<MinimalSoftwareWindow>,
    buffers: [Vec<Rgb8Pixel>; 2],
    front: usize,
    swapped: bool,
}

impl<W: Write> Tape<W> {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.frame * 1_000_000_000 / FPS)
    }

    fn push(&mut self, pixels: &[Rgb8Pixel]) {
        self.bytes.clear();
        for px in pixels {
            self.bytes.extend_from_slice(&[px.r, px.g, px.b]);
        }
        self.out.write_all(&self.bytes).expect("write frame");
        self.frame += 1;
    }

    fn black(&mut self, duration: Duration) {
        let black = vec![BLACK; WIDTH * HEIGHT];
        let until = self.now() + duration;
        while self.now() < until {
            self.push(&black);
        }
    }

    /// Advances Slint to the current frame time and renders into the back
    /// buffer, flipping only when something was drawn.
    fn render(&mut self) {
        CLOCK.with(|c| *c.borrow_mut() = self.now());
        slint::platform::update_timers_and_animations();
        let back = if self.swapped { 1 - self.front } else { self.front };
        let mut drawn = false;
        let buffer = &mut self.buffers[back];
        self.window.draw_if_needed(|renderer| {
            renderer.render(buffer, WIDTH);
            drawn = true;
        });
        if drawn {
            self.front = back;
        }
        let front = std::mem::take(&mut self.buffers[self.front]);
        self.push(&front);
        self.buffers[self.front] = front;
    }

    fn run(&mut self, duration: Duration, mut each: impl FnMut(Duration)) {
        let start = self.now();
        while self.now() - start < duration {
            each(self.now() - start);
            self.render();
        }
    }

    fn key(&self, text: &str, pressed: bool) {
        let text = SharedString::from(text);
        self.window.window().dispatch_event(if pressed {
            WindowEvent::KeyPressed { text }
        } else {
            WindowEvent::KeyReleased { text }
        });
    }

    /// Press and release, then let the result play for `after`.
    fn tap(&mut self, text: &str, after: Duration) {
        self.key(text, true);
        self.key(text, false);
        self.run(after, |_| {});
    }
}

fn key_text(key: Key) -> String {
    SharedString::from(key).to_string()
}

fn build_ui() -> AppWindow {
    let ui = AppWindow::new().expect("build ui");
    ui.set_version("0.3.5".into());
    ui.set_edition("Fleet".into());
    ui.set_disks(ModelRc::from(Rc::new(VecModel::from(sample::disks()))));
    ui.set_scan_status("3 storage devices ready".into());
    ui.set_image_status("IMAGE READY".into());
    ui.on_proceed({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_phase(Phase::Target);
            }
        }
    });
    ui.on_choose({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_phase(Phase::Confirm);
            }
        }
    });
    ui.on_back({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_phase(Phase::Target);
            }
        }
    });
    ui.on_begin_install({
        let ui = ui.as_weak();
        move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.set_summary_target("/dev/nvme0n1".into());
                ui.set_progress(0.0);
                ui.set_stage("PREPARING".into());
                ui.set_detail("".into());
                ui.set_phase(Phase::Install);
            }
        }
    });
    ui
}

/// Brings Slint's clock up to the present before a UI exists, or its timers
/// are already overdue when the first frame is drawn.
fn sync_clock<W: Write>(tape: &Tape<W>) {
    CLOCK.with(|c| *c.borrow_mut() = tape.now());
    slint::platform::update_timers_and_animations();
}

/// Black, then the installer from its first frame: the display settling, the
/// boot mark fading in, the first screen opening out of it, and Enter into
/// disk selection.
fn boot<W: Write>(tape: &mut Tape<W>) -> AppWindow {
    tape.black(Duration::from_millis(300));
    sync_clock(tape);

    let ui = build_ui();
    ui.show().expect("show");

    // Disk enumeration, image allocation and DRM setup run between creating
    // the UI and drawing its first frame. The screen stays black meanwhile.
    tape.black(Duration::from_millis(250));

    let decode = Duration::from_millis(2600);
    tape.run(Duration::from_millis(5700), |t| {
        // Reported as presented once the first frame is out, as the installer
        // does.
        if t > Duration::ZERO {
            ui.set_presented(true);
        }
        if t >= decode {
            ui.set_image_status("IMAGE READY".into());
        } else {
            let percent = t.as_secs_f64() / decode.as_secs_f64() * 100.0;
            ui.set_image_status(format!("PREPARING IMAGE  {percent:.0}%").into());
        }
    });

    tape.tap(&key_text(Key::Return), Duration::from_millis(1400));
    ui
}

/// Disk selection at rest, already past the display settle.
fn at_target<W: Write>(tape: &mut Tape<W>, style: i32) -> AppWindow {
    sync_clock(tape);
    let ui = build_ui();
    ui.set_confirm_style(style);
    ui.set_phase(Phase::Target);
    ui.set_presented(true);
    ui.show().expect("show");
    tape.run(Duration::from_millis(1300), |_| {});
    ui
}

/// The write running, then finishing into the summary.
fn install<W: Write>(tape: &mut Tape<W>, ui: &AppWindow, to_summary: bool) {
    let length = Duration::from_millis(if to_summary { 3200 } else { 2200 });
    tape.run(length, |t| {
        let p = (t.as_secs_f32() - 0.4).max(0.0) / 2.4;
        ui.set_progress(p.min(1.0));
        ui.set_stage(if p < 1.0 { "WRITING" } else { "VERIFYING" }.into());
        ui.set_detail(if p > 0.0 && p < 1.0 { "1.84 GB/s" } else { "" }.into());
    });
    if !to_summary {
        return;
    }
    ui.set_summary_headline("Installation complete".into());
    ui.set_summary_elapsed("2.31 s".into());
    ui.set_summary_written("1.18 GiB".into());
    ui.set_summary_average("548 MB/s".into());
    ui.set_summary_peak("1.92 GB/s".into());
    ui.set_summary_target("/dev/nvme0n1  Samsung SSD 990 PRO 2TB".into());
    ui.set_summary_image("carbideos-fleet 0.3.5".into());
    ui.set_phase(Phase::Summary);
    tape.run(Duration::from_millis(3000), |_| {});
}

/// Hold: a hold released early, then a full hold into the write.
fn confirm_hold<W: Write>(tape: &mut Tape<W>) {
    let ui = at_target(tape, 0);
    let enter = key_text(Key::Return);
    tape.tap(&enter, Duration::from_millis(1100));

    tape.key(&enter, true);
    tape.run(Duration::from_millis(600), |_| {});
    tape.key(&enter, false);
    tape.run(Duration::from_millis(1100), |_| {});

    tape.key(&enter, true);
    tape.run(Duration::from_millis(1800), |_| {});
    tape.key(&enter, false);
    install(tape, &ui, false);
}

/// Type the code: a typo, then the code typed out into the write.
fn confirm_type<W: Write>(tape: &mut Tape<W>) {
    let ui = at_target(tape, 1);
    tape.tap(&key_text(Key::Return), Duration::from_millis(1200));

    for c in ["0", "4", "7"] {
        tape.tap(c, Duration::from_millis(260));
    }
    tape.run(Duration::from_millis(900), |_| {});
    for c in ["0", "4", "8", "1"] {
        tape.tap(c, Duration::from_millis(300));
    }
    tape.run(Duration::from_millis(700), |_| {});
    install(tape, &ui, false);
}

/// Countdown: armed and aborted, then armed and run out into the write.
fn confirm_countdown<W: Write>(tape: &mut Tape<W>) {
    let ui = at_target(tape, 2);
    let enter = key_text(Key::Return);
    tape.tap(&enter, Duration::from_millis(1200));

    tape.tap(&enter, Duration::from_millis(2300));
    tape.tap(&key_text(Key::Escape), Duration::from_millis(1100));
    tape.tap(&enter, Duration::from_millis(5900));
    install(tape, &ui, false);
}

/// Boot to finish: every screen and every transition in one pass.
fn flow<W: Write>(tape: &mut Tape<W>) {
    let ui = boot(tape);
    let (up, down, enter) = (key_text(Key::UpArrow), key_text(Key::DownArrow), key_text(Key::Return));
    tape.tap(&down, Duration::from_millis(700));
    tape.tap(&up, Duration::from_millis(900));
    tape.tap(&enter, Duration::from_millis(1300));
    for c in ["0", "4", "8", "1"] {
        tape.tap(c, Duration::from_millis(280));
    }
    tape.run(Duration::from_millis(700), |_| {});
    install(tape, &ui, true);
}

fn main() {
    let scene = std::env::args().nth(1).unwrap_or_else(|| "boot".into());

    // Hardware double-buffers, so partial repaints are the default. The others
    // are for telling a redraw bug apart from a design one.
    let buffering = match std::env::var("CARBIDE_RECORD_BUFFERS").as_deref() {
        Ok("new") => RepaintBufferType::NewBuffer,
        Ok("reused") => RepaintBufferType::ReusedBuffer,
        _ => RepaintBufferType::SwappedBuffers,
    };
    let window = MinimalSoftwareWindow::new(buffering);
    slint::platform::set_platform(Box::new(Recorder {
        window: window.clone(),
    }))
    .expect("set platform");
    window.set_size(PhysicalSize::new(WIDTH as u32, HEIGHT as u32));

    let stdout = std::io::stdout();
    let mut tape = Tape {
        out: BufWriter::with_capacity(WIDTH * HEIGHT * 3, stdout.lock()),
        frame: 0,
        bytes: Vec::with_capacity(WIDTH * HEIGHT * 3),
        window,
        buffers: [vec![BLACK; WIDTH * HEIGHT], vec![BLACK; WIDTH * HEIGHT]],
        front: 0,
        swapped: buffering == RepaintBufferType::SwappedBuffers,
    };

    match scene.as_str() {
        "boot" => drop(boot(&mut tape)),
        "confirm-hold" => confirm_hold(&mut tape),
        "confirm-type" => confirm_type(&mut tape),
        "confirm-countdown" => confirm_countdown(&mut tape),
        "flow" => flow(&mut tape),
        other => {
            eprintln!("unknown scene {other}; expected {SCENES}");
            std::process::exit(2);
        }
    }

    tape.out.flush().expect("flush");
    eprintln!("{scene}: {} frames at {FPS} fps", tape.frame);
}
