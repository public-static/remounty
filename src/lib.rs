//! Remounty — a menu bar app that re-mounts read-only NTFS volumes read-write
//! with ntfs-3g and macFUSE.
//!
//! The crate builds two executables from this library: the menu bar app
//! (`remounty`, see [`app_main`]) and the privileged helper
//! (`remounty-helper`, see [`helper::main`]).

mod app;
mod authz;
mod cmd;
mod deps;
mod disks;
mod error;
mod finder;
pub mod helper;
mod helper_client;
mod helper_install;
mod helper_proto;
mod icon;
mod instance;
mod logging;
mod login;
mod menu;
mod mounts;
mod naming;
mod notify;
mod ops;
mod paths;
mod privileged;
mod settings;
mod trust;
mod ui;
mod watcher;
mod worker;

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tao::platform::run_return::EventLoopExtRunReturn;
use tray_icon::menu::MenuEvent;

use app::{App, AppEvent, Sink};

/// Entry point of the menu bar app.
pub fn app_main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--scan") {
        print_scan();
        return ExitCode::SUCCESS;
    }
    logging::init();
    std::panic::set_hook(Box::new(|info| {
        logging::write("PANIC", &info.to_string());
    }));
    log_info!("Remounty {} starting", env!("CARGO_PKG_VERSION"));

    let _lock = match instance::acquire() {
        Ok(instance::Acquire::Acquired(lock)) => lock,
        Ok(instance::Acquire::AlreadyRunning) => {
            log_info!("Another instance is already running");
            ui::notify(paths::APP_NAME, "Remounty is already running in the menu bar.");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            log_error!("{err}");
            ui::alert(
                ui::Level::Critical,
                "Remounty could not start",
                &err.to_string(),
                &["OK"],
                ui::DEFAULT_GIVE_UP,
            );
            return ExitCode::FAILURE;
        }
    };

    run();
    log_info!("Remounty stopped");
    ExitCode::SUCCESS
}

fn run() {
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let proxy = Mutex::new(event_loop.create_proxy());
    let sink: Sink = Arc::new(move |event| match proxy.lock() {
        Ok(proxy) => {
            if proxy.send_event(event).is_err() {
                log_warn!("Event loop is gone; dropping event");
            }
        }
        Err(_) => log_error!("Event proxy lock poisoned; dropping event"),
    });

    {
        let sink = sink.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| sink(AppEvent::Menu(event.id.0))));
    }

    let mut app: Option<App> = None;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // A panic must never unwind into AppKit (that would abort the process
        // in the middle of whatever is going on); log it and keep running.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match event {
            Event::NewEvents(StartCause::Init) => match App::start(sink.clone()) {
                Ok(started) => {
                    app = Some(started);
                    false
                }
                Err(err) => {
                    log_error!("Startup failed: {err}");
                    ui::alert(
                        ui::Level::Critical,
                        "Remounty could not start",
                        &err.to_string(),
                        &["OK"],
                        ui::DEFAULT_GIVE_UP,
                    );
                    true
                }
            },
            Event::UserEvent(user_event) => match app.as_mut() {
                Some(app) => app.handle(user_event),
                None => false,
            },
            _ => false,
        }));
        match outcome {
            Ok(true) => *control_flow = ControlFlow::Exit,
            Ok(false) => {}
            Err(_) => {
                log_error!("Recovered from an internal error in the event handler");
                notify::post(
                    paths::APP_NAME,
                    "An internal error occurred. Please check the volumes in the menu and the log.",
                    None,
                );
            }
        }
    });
}

/// `remounty --scan`: prints what Remounty sees, without changing anything.
fn print_scan() {
    let deps = deps::detect(settings::Store::new().load().ntfs3g_path.as_deref());
    match &deps.ntfs3g {
        Ok(path) => println!("ntfs-3g: {}", path.display()),
        Err(reason) => println!("ntfs-3g: {reason}"),
    }
    match &deps.macfuse {
        Ok(()) => println!("macFUSE: ok"),
        Err(reason) => println!("macFUSE: {reason}"),
    }
    println!("helper: {}", helper_client::status());
    match disks::scan() {
        Ok(scan) => {
            if scan.volumes.is_empty() {
                println!("No NTFS volumes found.");
            }
            for vol in &scan.volumes {
                println!(
                    "{} \"{}\" {} [{}] media={} id={} path={}",
                    vol.bsd_name,
                    vol.name,
                    disks::format_size(vol.size),
                    menu::describe_state(&vol.state),
                    vol.media,
                    vol.identity.as_deref().unwrap_or("-"),
                    vol.state
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "-".into())
                );
            }
            for orphan in &scan.orphans {
                println!("stale mount: {} (from {})", orphan.on.display(), orphan.from);
            }
        }
        Err(err) => println!("Scan failed: {err}"),
    }
}
