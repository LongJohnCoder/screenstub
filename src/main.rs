#![recursion_limit = "1024"]

extern crate tokio_qapi as qapi;
extern crate input_linux as input;
extern crate screenstub_uinput as uinput;
extern crate screenstub_config as config;
extern crate screenstub_event as event;
extern crate screenstub_qemu as qemu;
extern crate screenstub_ddc as ddc;
extern crate screenstub_x as x;

use std::process::exit;
use std::pin::Pin;
use std::sync::Arc;
use std::io::{self, Write};
use futures::channel::{mpsc, oneshot};
use futures::channel::mpsc as un_mpsc;
use futures::{stream, future, TryFutureExt, FutureExt, StreamExt, SinkExt};
use futures::stream::FusedStream;
use futures::lock::Mutex;
use failure::{Error, format_err};
use log::{warn, error, trace};
use clap::{Arg, App, SubCommand, AppSettings};
use input::{InputId, Key, RelativeAxis, AbsoluteAxis, InputEvent, EventKind};
use config::{Config, ConfigEvent};
use event::{Hotkey, UserEvent, ProcessedXEvent};
use qemu::Qemu;
use route::Route;
use inputs::Inputs;
use process::Process;
#[cfg(feature = "with-ddcutil")]
use ddc::ddcutil::Monitor;
use x::XRequest;

mod route;
mod grab;
mod filter;
mod inputs;
mod exec;
mod process;

const EVENT_BUFFER: usize = 8;

#[tokio::main]
async fn main() {
    match main_result().await {
        Ok(code) => exit(code),
        Err(e) => {
            let _ = writeln!(io::stderr(), "{:?} {}", e, e);
            exit(1);
        },
    }
}

async fn main_result() -> Result<i32, Error> {
    env_logger::init();

    let app = App::new("screenstub")
        .version(env!("CARGO_PKG_VERSION"))
        .author("arcnmx")
        .about("A software KVM")
        .arg(Arg::with_name("config")
            .short("c")
            .long("config")
            .value_name("CONFIG")
            .takes_value(true)
            .help("Configuration TOML file")
        ).subcommand(SubCommand::with_name("x")
            .about("Start the KVM with a fullscreen X window")
        ).subcommand(SubCommand::with_name("detect")
            .about("Detect available DDC/CI displays and their video inputs")
        ).subcommand(SubCommand::with_name("input")
            .about("Change the configured monitor input")
            .arg(Arg::with_name("confirm")
                 .short("c")
                 .long("confirm")
                 .help("Check that the VM is running before switching input")
            ).arg(Arg::with_name("input")
                .value_name("DEST")
                .takes_value(true)
                .required(true)
                .possible_values(&["host", "guest"])
                .help("Switch to either the host or guest monitor input")
            )
        ).setting(AppSettings::SubcommandRequiredElseHelp);

    let matches = app.get_matches();
    let config = if let Some(config) = matches.value_of("config") {
        use std::fs::File;

        let f = File::open(config)?;
        serde_yaml::from_reader(f)?
    } else {
        Config::default()
    };

    match matches.subcommand() {
        ("x", Some(..)) => {
            let mut config = config.get(0).ok_or_else(|| format_err!("expected a screen config"))?.clone();

            let (mut x_sender, mut x_receiver) = mpsc::channel(0x20);
            let (mut xreq_sender, mut xreq_receiver) = mpsc::channel(0x08);
            let x = x::XContext::xmain()?;
            let xmain = tokio::spawn(async move {
                let mut x = x.fuse();
                loop {
                    futures::select! {
                        req = xreq_receiver.next() => if let Some(req) = req {
                            let _ = x.send(req).await;
                        },
                        event = x.next() => match event {
                            Some(Ok(event)) => {
                                let _ = x_sender.send(event).await;
                            },
                            Some(Err(e)) => {
                                error!("X Error: {}: {:?}", e, e);
                                break
                            },
                            None => {
                                break
                            },
                        },
                        complete => break,
                    }
                }
            }).map_err(From::from);

            let qemu = Arc::new(Qemu::new(config.qemu.qmp_socket, config.qemu.ga_socket));

            let inputs = Inputs::new(qemu.clone(), config.monitor, config.host_source, config.guest_source, config.ddc.host, config.ddc.guest);

            let (mut event_sender, mut event_recv) = un_mpsc::channel(EVENT_BUFFER);
            let (error_sender, mut error_recv) = un_mpsc::channel(1);

            if let Some(driver) = config.qemu.driver {
                config.qemu.keyboard_driver = driver.clone();
                config.qemu.relative_driver = driver.clone();
                config.qemu.absolute_driver = driver;
            }
            let process = Process::new(
                config.qemu.routing, config.qemu.keyboard_driver, config.qemu.relative_driver, config.qemu.absolute_driver, config.exit_events,
                qemu.clone(), inputs, xreq_sender.clone(), event_sender.clone(), error_sender.clone(),
            );

            process.devices_init().await?;
            process.set_is_mouse(false).await?; // TODO: config option to start up in relative mode instead

            let uinput_id = InputId {
                bustype: input::sys::BUS_VIRTUAL,
                vendor: 0x16c0,
                product: 0x05df,
                version: 1,
            };

            let repeat = false;
            let bus = None;
            let mut route_keyboard = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-kbd".into(), bus.clone(), repeat);
            if let Some(builder) = route_keyboard.builder() {
                builder
                    .name("screenstub-kbd")
                    .x_config_key(repeat)
                    .id(&uinput_id);
            }
            let mut events_keyboard = route_keyboard.spawn(error_sender.clone());

            let mut route_relative = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-mouse".into(), bus.clone(), repeat);
            if let Some(builder) = route_relative.builder() {
                builder
                    .name("screenstub-mouse")
                    .x_config_rel()
                    .id(&uinput_id);
            }
            let mut events_relative = route_relative.spawn(error_sender.clone());

            let mut route_absolute = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-tablet".into(), bus, repeat);
            if let Some(builder) = route_absolute.builder() {
                builder
                    .name("screenstub-tablet")
                    .x_config_abs()
                    .id(&uinput_id);
            }
            let mut events_absolute = route_absolute.spawn(error_sender.clone());

            let x_filter = process.x_filter();

            let process = Arc::new(process);

            let (mut user_sender, user_receiver) = un_mpsc::channel::<Arc<ConfigEvent>>(0x08);
            let mut user_receiver = user_receiver
                .map({
                    let process = process.clone();
                    move |event| process.process_user_event(&event)
                });

            let mut events = event::Events::new();
            config.hotkeys.into_iter()
                .map(convert_hotkey)
                .for_each(|(hotkey, on_press)| events.add_hotkey(hotkey, on_press));
            config.key_remap.into_iter().for_each(|(from, to)| events.add_remap(from, to));

            let events = Arc::new(Mutex::new(events));

            let (event_loop, event_loop_abort) = future::abortable({
                let events = events.clone();
                let process = process.clone();
                let mut user_sender = user_sender.clone();
                async move {
                    while let Some(event) = event_recv.next().await {
                        let mut events = events.lock().await;
                        let inputevent = events.map_input_event(event.clone());
                        let user_sender = &mut user_sender;
                        let f1 = async move {
                            for e in events.process_input_event(&event) {
                                let _ = user_sender.send(e.clone()).await;
                            }
                        };
                        let is_mouse = process.is_mouse();

                        let events_keyboard = &mut events_keyboard;
                        let events_relative = &mut events_relative;
                        let events_absolute = &mut events_absolute;
                        let f2 = async move {
                            match map_event_kind(&inputevent, is_mouse) {
                                EventKind::Key => {
                                    let _ = events_keyboard.send(inputevent).await;
                                },
                                EventKind::Relative => {
                                    let _ = events_relative.send(inputevent).await;
                                },
                                EventKind::Absolute => {
                                    let _ = events_absolute.send(inputevent).await;
                                },
                                EventKind::Synchronize => {
                                    let _ = future::try_join3(
                                        events_keyboard.send(inputevent),
                                        events_relative.send(inputevent),
                                        events_absolute.send(inputevent)
                                    ).await;
                                },
                                _ => (),
                            }
                        };
                        let _ = future::join(f1, f2).await;
                    }
                }
            });
            let event_loop = tokio::spawn(event_loop.map(drop))
                .map_err(Error::from);

            let (xevent_exit_send, xevent_exit_recv) = oneshot::channel();
            let mut xevent_exit_recv = xevent_exit_recv.fuse();
            let xevent_loop = tokio::spawn({
                async move {
                    while let Some(xevent) = x_receiver.next().await {
                        let events = events.lock().await.process_x_event(&xevent);
                        for e in events {
                            match e {
                                ProcessedXEvent::UserEvent(e) => {
                                    let _ = user_sender.send(convert_user_event(e)).await;
                                },
                                ProcessedXEvent::InputEvent(e) if x_filter.try_lock().unwrap().filter_event(&e) => {
                                    let _ = event_sender.send(e).await;
                                },
                                ProcessedXEvent::InputEvent(_) => (),
                            }
                        }
                    }

                    let _ = xevent_exit_send.send(());
                }
            }).map_err(From::from);

            let res = loop {
                let mut qmp_poll = future::poll_fn(|cx| qemu.poll_qmp_events(cx)).fuse();
                futures::select! {
                    _ = xevent_exit_recv => break Ok(()),
                    error = error_recv.next() => if let Some(error) = error {
                        break Err(error)
                    },
                    event = user_receiver.next() => if let Some(event) = event {
                        tokio::spawn(async move {
                            match Pin::from(event).await {
                                Err(e) =>
                                    warn!("User event failed {} {:?}", e, e),
                                Ok(()) => (),
                            }
                        });
                    },
                    e = qmp_poll => {
                        trace!("Ignoring QMP event {:?}", e);
                    },
                }
            };

            let _ = xreq_sender.send(XRequest::Quit).await; // ensure we kill x
            drop(xreq_sender);
            drop(process);

            // seal off senders
            event_loop_abort.abort();
            future::try_join3(
                event_loop,
                xevent_loop,
                xmain,
            ).await?;

            res.map(|()| 0)
        },
        #[cfg(feature = "with-ddcutil")]
        ("detect", Some(..)) => {
            Monitor::enumerate()?.into_iter().for_each(|m| {
                let info = m.info().unwrap();
                let inputs = m.inputs().unwrap();
                let current_input = m.our_input().unwrap();
                println!("Manufacturer: {}\nModel: {}\nSerial: {}",
                    info.manufacturer_id(), info.model_name(), info.serial_number()
                );
                inputs.into_iter().for_each(|i|
                    println!("  Input: {} = 0x{:02x}{}", i.1, i.0,
                        if *i.0 == current_input { " (Current)" } else { "" }
                    )
                );
            });

            Ok(0)
        },
        #[cfg(feature = "with-ddcutil")]
        ("input", Some(matches)) => {
            let config = config.get(0).ok_or_else(|| format_err!("expected a screen config"))?.clone();

            let qemu = Arc::new(Qemu::new(config.qemu.qmp_socket, config.qemu.ga_socket));
            let inputs = Inputs::new(qemu, config.monitor, config.host_source, config.guest_source, config.ddc.host, config.ddc.guest);

            match matches.value_of("input") {
                Some("host") => inputs.show_host().await,
                Some("guest") => inputs.show_guest().await, // TODO: bypass check for guest agent
                _ => unreachable!("unknown input to switch to"),
            }.map(|_| 0)
        },
        _ => unreachable!("unknown command"),
    }
}

fn axis_is_relative(axis: RelativeAxis) -> bool {
    match axis {
        RelativeAxis::X | RelativeAxis::Y => true,
        _ => false,
    }
}

fn axis_is_absolute(axis: AbsoluteAxis) -> bool {
    match axis {
        AbsoluteAxis::X | AbsoluteAxis::Y => true,
        _ => false,
    }
}

fn convert_user_event(event: UserEvent) -> Arc<ConfigEvent> {
    Arc::new(match event {
        UserEvent::Quit => ConfigEvent::Exit,
        UserEvent::ShowGuest => ConfigEvent::ShowGuest,
        UserEvent::ShowHost => ConfigEvent::ShowHost,
        UserEvent::UnstickGuest => ConfigEvent::UnstickGuest,
        UserEvent::UnstickHost => ConfigEvent::UnstickHost,
    })
}

fn convert_hotkey(hotkey: config::ConfigHotkey) -> (Hotkey<Arc<ConfigEvent>>, bool) {
    (
        Hotkey::new(hotkey.triggers, hotkey.modifiers, hotkey.events.into_iter().map(Arc::new)),
        !hotkey.on_release,
    )
}

fn map_event_kind(inputevent: &InputEvent, is_mouse: bool) -> EventKind {
    match inputevent.kind {
        EventKind::Key if Key::from_code(inputevent.code).map(|k| k.is_button()).unwrap_or(false) =>
            if is_mouse {
                EventKind::Relative
            } else {
                EventKind::Absolute
            },
        EventKind::Key =>
            EventKind::Key,
        EventKind::Absolute if inputevent.code == AbsoluteAxis::Volume as u16 =>
            EventKind::Key, // is this right?
        EventKind::Relative if RelativeAxis::from_code(inputevent.code).map(|a| axis_is_relative(a)).unwrap_or(false) =>
            EventKind::Relative,
        EventKind::Absolute if AbsoluteAxis::from_code(inputevent.code).map(|a| axis_is_absolute(a)).unwrap_or(false) =>
            EventKind::Absolute,
        EventKind::Relative | EventKind::Absolute =>
            if is_mouse {
                EventKind::Relative
            } else {
                EventKind::Absolute
            },
        EventKind::Synchronize =>
            EventKind::Synchronize,
        kind => {
            warn!("unforwarded event {:?}", kind);
            kind
        },
    }
}
