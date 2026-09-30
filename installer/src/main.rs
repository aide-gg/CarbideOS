// SPDX-License-Identifier: AGPL-3.0-or-later
//! CarbideOS installer.
//!
//! Renders to DRM/KMS with no compositor, picks a target disk, and writes the
//! image. Everything downstream of that first reboot — layout growth, state
//! encryption, Secure Boot enrollment, integrity — is already the operating
//! system's job and is left to it.

mod disk;
mod image;
mod install;
mod sys;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

const DEFAULT_PAYLOAD: &str = "/usr/share/carbideos/image.raw.zst";
const DEFAULT_META: &str = "/usr/share/carbideos/image.meta";
const POLL: Duration = Duration::from_millis(50);

fn payload_path() -> PathBuf {
    std::env::var_os("CARBIDE_PAYLOAD")
        .map(PathBuf::from)
        .unwrap_or_else(|| DEFAULT_PAYLOAD.into())
}

fn meta_path() -> PathBuf {
    std::env::var_os("CARBIDE_META")
        .map(PathBuf::from)
        .unwrap_or_else(|| DEFAULT_META.into())
}

/// Starts decoding the carried image immediately. It runs behind the attract
/// screen and the disk picker, so the write that follows is never waiting on
/// decompression.
fn start_image() -> Result<Arc<image::Image>, String> {
    let meta = image::Meta::read(&meta_path())
        .map_err(|e| format!("reading {}: {e}", meta_path().display()))?;
    image::Image::start(&payload_path(), meta)
        .map_err(|e| format!("reading {}: {e}", payload_path().display()))
}

fn format_rate(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1e9 {
        format!("{:.2} GB/s", bytes_per_second / 1e9)
    } else {
        format!("{:.0} MB/s", bytes_per_second / 1e6)
    }
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        format!("{}m {:04.1}s", (secs / 60.0) as u64, secs % 60.0)
    }
}

fn to_row(d: &disk::Disk) -> DiskInfo {
    let code = d.confirm_code();
    DiskInfo {
        code_chars: ModelRc::new(VecModel::from(
            code.chars.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )),
        code_kind: code.kind.into(),
        code_context: code.context.into(),
        node: d.node.clone().into(),
        model: d.model.clone().into(),
        size: disk::format_size(d.bytes).into(),
        bus: d.bus.clone().into(),
        kind: d.kind.clone().into(),
        serial: d.serial.clone().into(),
        eligible: d.eligible(),
        boot_media: d.boot_media,
        note: d.note().into(),
    }
}

/// Unattended write. Runs the same engine the interface drives, with no DRM
/// device and no operator, so the write path can be exercised against a loop
/// device in CI and used for scripted provisioning.
fn write_headless(target: &str) -> std::process::ExitCode {
    let image = match start_image() {
        Ok(image) => image,
        Err(message) => {
            eprintln!("carbide-installer: {message}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let progress = install::Progress::new();
    let node = PathBuf::from(target);

    {
        let progress = Arc::clone(&progress);
        let image = Arc::clone(&image);
        std::thread::spawn(move || install::run(&node, image, progress));
    }

    let started = Instant::now();
    let mut last_stage = None;
    while !progress.finished() {
        let stage = progress.stage();
        if last_stage != Some(stage) {
            eprintln!("{}", stage.label());
            last_stage = Some(stage);
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    if let Some(message) = progress.error() {
        eprintln!("carbide-installer: {message}");
        return std::process::ExitCode::FAILURE;
    }

    let elapsed = started.elapsed();
    let total = progress.total();
    eprintln!(
        "wrote {} to {target} in {} ({})",
        disk::format_bytes_exact(total),
        format_elapsed(elapsed),
        format_rate(total as f64 / elapsed.as_secs_f64().max(1e-6))
    );
    std::process::ExitCode::SUCCESS
}

/// Opens the display, giving it a few seconds to appear. While a GPU driver
/// takes over from simpledrm there is briefly no DRM device to open at all.
fn create_window() -> Result<AppWindow, slint::PlatformError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut logged = false;
    loop {
        match AppWindow::new() {
            Ok(ui) => return Ok(ui),
            Err(e) if Instant::now() < deadline => {
                if !logged {
                    eprintln!("carbide-installer: display not ready, retrying: {e}");
                    logged = true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(index) = args.iter().position(|a| a == "--write") {
        let Some(target) = args.get(index + 1) else {
            eprintln!("Usage: carbide-installer --write <device>");
            std::process::exit(2);
        };
        std::process::exit(match write_headless(target) {
            code if code == std::process::ExitCode::SUCCESS => 0,
            _ => 1,
        });
    }

    let image = start_image();
    let ui = create_window()?;

    let version: SharedString = std::env::var("CARBIDE_VERSION")
        .unwrap_or_else(|_| String::new())
        .into();
    ui.set_version(version);
    let edition: SharedString = std::env::var("CARBIDE_EDITION")
        .unwrap_or_else(|_| String::new())
        .into();
    ui.set_edition(edition);

    // Owned here and mirrored into the Slint model, because the UI only ever
    // carries display strings and the writer needs the real device node.
    let disks: Rc<RefCell<Vec<disk::Disk>>> = Rc::new(RefCell::new(Vec::new()));
    let rows: Rc<VecModel<DiskInfo>> = Rc::new(VecModel::default());
    ui.set_disks(ModelRc::from(rows.clone()));

    let refresh = {
        let ui = ui.as_weak();
        let disks = disks.clone();
        let rows = rows.clone();
        move || {
            let found = disk::enumerate();
            let usable = found.iter().filter(|d| d.eligible()).count();
            rows.set_vec(found.iter().map(to_row).collect::<Vec<_>>());
            *disks.borrow_mut() = found;

            if let Some(ui) = ui.upgrade() {
                let first = disks
                    .borrow()
                    .iter()
                    .position(|d| d.eligible())
                    .unwrap_or(0);
                ui.set_selected(first as i32);
                ui.set_scan_status(
                    match usable {
                        0 => "No eligible storage device found".to_string(),
                        1 => "1 storage device ready".to_string(),
                        n => format!("{n} storage devices ready"),
                    }
                    .into(),
                );
            }
        }
    };
    refresh();

    {
        let refresh = refresh.clone();
        ui.on_rescan(refresh);
    }

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

    ui.on_reboot(|| sys::reboot());
    ui.on_poweroff(|| sys::power_off());

    // Kept alive for the process lifetime; a dropped Timer stops firing.
    let poll_timer = Rc::new(Timer::default());

    let ready_image = image.as_ref().ok().map(Arc::clone);

    ui.on_begin_install({
        let ui = ui.as_weak();
        let disks = disks.clone();
        let poll_timer = poll_timer.clone();
        let ready_image = ready_image.clone();

        move |index| {
            let Some(ui) = ui.upgrade() else { return };
            let Some(target) = disks.borrow().get(index as usize).cloned() else {
                return;
            };
            if !target.eligible() {
                return;
            }
            let Some(image) = ready_image.clone() else {
                return;
            };
            let label = image.label.clone();

            ui.set_summary_target(target.node.clone().into());
            ui.set_progress(0.0);
            ui.set_stage(install::Stage::Preparing.label().into());
            ui.set_detail(SharedString::new());
            ui.set_phase(Phase::Install);

            let progress = install::Progress::new();
            let started = Instant::now();

            {
                let progress = Arc::clone(&progress);
                let node = PathBuf::from(&target.node);
                std::thread::spawn(move || {
                    install::run(&node, image, progress);
                });
            }

            let peak = Rc::new(RefCell::new(0.0f64));
            let last = Rc::new(RefCell::new((Instant::now(), 0u64)));

            poll_timer.start(TimerMode::Repeated, POLL, {
                let ui = ui.as_weak();
                let timer = Rc::downgrade(&poll_timer);
                let progress = Arc::clone(&progress);
                let peak = peak.clone();
                let last = last.clone();
                let target = target.clone();
                let label = label.clone();

                move || {
                    let Some(ui) = ui.upgrade() else { return };
                    let written = progress.written();
                    let stage = progress.stage();

                    ui.set_progress(progress.fraction());
                    ui.set_stage(stage.label().into());

                    // Instantaneous rate over the poll window. The average is
                    // computed from the total at the end instead, so a slow
                    // discard or flush does not skew it.
                    let (prev_at, prev_bytes) = *last.borrow();
                    let dt = prev_at.elapsed().as_secs_f64();
                    if dt >= 0.2 {
                        let rate = (written.saturating_sub(prev_bytes)) as f64 / dt;
                        if rate > *peak.borrow() {
                            *peak.borrow_mut() = rate;
                        }
                        *last.borrow_mut() = (Instant::now(), written);
                        if stage == install::Stage::Writing {
                            ui.set_detail(format_rate(rate).into());
                        }
                    }

                    if !progress.finished() {
                        return;
                    }
                    if let Some(timer) = timer.upgrade() {
                        timer.stop();
                    }

                    let elapsed = started.elapsed();
                    if let Some(message) = progress.error() {
                        ui.set_summary_headline("Installation failed".into());
                        ui.set_summary_message(message.into());
                        ui.set_phase(Phase::Failure);
                    } else {
                        let total = progress.total();
                        let average = total as f64 / elapsed.as_secs_f64().max(1e-6);
                        ui.set_progress(1.0);
                        ui.set_summary_headline("Installation complete".into());
                        ui.set_summary_elapsed(format_elapsed(elapsed).into());
                        ui.set_summary_written(disk::format_bytes_exact(total).into());
                        ui.set_summary_average(format_rate(average).into());
                        ui.set_summary_peak(format_rate(*peak.borrow()).into());
                        ui.set_summary_target(format!("{}  {}", target.node, target.model).into());
                        ui.set_summary_image(label.clone().into());
                        ui.set_phase(Phase::Summary);
                    }
                }
            });
        }
    });

    // Readiness ticker. Decoding overlaps the attract screen and the picker,
    // so this is usually settled before anyone reaches the confirm step, but
    // an operator who moves faster than the decoder should see why the write
    // is waiting rather than watching a stalled bar.
    let ready_timer = Rc::new(Timer::default());
    if let Ok(image) = &image {
        ready_timer.start(TimerMode::Repeated, Duration::from_millis(120), {
            let ui = ui.as_weak();
            let timer = Rc::downgrade(&ready_timer);
            let image = Arc::clone(image);
            move || {
                let Some(ui) = ui.upgrade() else { return };
                let settled = image.error().is_some() || image.complete();
                if let Some(message) = image.error() {
                    ui.set_image_status(format!("IMAGE FAILED  {message}").into());
                } else if image.complete() {
                    ui.set_image_status("IMAGE READY".into());
                } else {
                    ui.set_image_status(
                        format!("PREPARING IMAGE  {:.0}%", image.fraction() * 100.0).into(),
                    );
                }
                if settled && let Some(timer) = timer.upgrade() {
                    timer.stop();
                }
            }
        });
    }

    // Report the first frame as presented. A message posted before the loop
    // runs is received while it waits after frame one, and delivered on the
    // following turn, before frame two is drawn.
    let weak = ui.as_weak();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_presented(true);
        }
    })
    .map_err(|e| slint::PlatformError::Other(e.to_string()))?;

    // A payload that cannot even be opened is terminal, and saying so beats
    // an attract screen that never advances.
    if let Err(message) = &image {
        ui.set_summary_headline("No installable image".into());
        ui.set_summary_message(message.clone().into());
        ui.set_phase(Phase::Failure);
    }

    ui.run()
}
