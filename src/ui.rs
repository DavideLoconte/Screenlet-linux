//! Screenlet's window: the tablets (on the cable, the network, or their own
//! Wi-Fi Direct group) with what can be done with each, how the desktop
//! reaches them, and for those who measure, the advanced settings and the
//! latency live. As Screenlet for macOS's window.

use crate::cable::{self, Cable};
use crate::link::paired;
use crate::nearby::{self, Tablet};
use crate::session::{self, Path, Report, Session};
use crate::settings::Settings;
use crate::sizes;
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HELP_MODE: &str = "Extend adds a display made at the chosen resolution, with nothing scaled: the sharpest. Mirror shows one of this PC's monitors on the tablet, fitted to its proportions. The latency is the same.";
const HELP_RESOLUTION: &str = "Fewer pixels mean less work for the encoder and less data on the network: less latency, softer text. Automatic uses the native resolution over the cable and, over Wi-Fi, one that suits the tablet, at most 1920 pixels wide on the densest screens.";
const HELP_PRIORITY: &str = "Lowest latency sends light frames that arrive sooner. Sharpest allows more bandwidth and larger frames, for a more faithful picture that takes longer to arrive. The bandwidth goes down by itself when the network falls behind, and back up when it is free.";
const HELP_CONNECTION: &str = "Automatic uses the cable when the tablet is on it, Wi-Fi otherwise. The cable gives the lowest and steadiest latency, and charges the tablet.";
const HELP_STATS: &str = "Measured by the tablet with the clocks of both matched: from this PC capturing a frame to the tablet having it decoded, and its stages. The screen adds one or two refreshes.";

/// What the threads tell the window.
enum Event {
    Ask(String, String, Sender<bool>),
    Paired(Result<String, String>),
    Cable(Result<Arc<Cable>, String>, Then),
}

#[derive(Clone, Copy)]
enum Then {
    Show,
    Pair,
}

struct App {
    window: adw::ApplicationWindow,
    settings: Settings,
    session: Option<(Session, String)>,
    cable: Option<Arc<Cable>>,
    plugged: bool,
    checked: Instant,
    nearby: Arc<Mutex<HashMap<String, Tablet>>>,
    events: (Sender<Event>, Receiver<Event>),
    busy: Option<String>,
    status: gtk::Label,
    devices: adw::PreferencesGroup,
    rows: Vec<adw::ActionRow>,
    shown: String,
    stats: [adw::ActionRow; 5],
    resolution: adw::ComboRow,
    resolutions: Vec<String>,
    bus: Option<gtk::gio::DBusConnection>,
    announced: (bool, String),
}

type Shared = Rc<RefCell<App>>;

const BUS_XML: &str = r#"<node><interface name="io.github.davideloconte.Screenlet1">
  <method name="Connect"/>
  <method name="Disconnect"/>
  <method name="ShowWindow"/>
  <method name="Quit"/>
  <property name="Active" type="b" access="read"/>
  <property name="Title" type="s" access="read"/>
</interface></node>"#;
const BUS_PATH: &str = "/io/github/davideloconte/Screenlet";

/// background: started by the Quick Settings toggle or at login, with no window until asked.
pub fn run(background: bool) -> glib::ExitCode {
    let app = adw::Application::builder().application_id("io.github.davideloconte.Screenlet").build();
    let shared: Rc<RefCell<Option<Shared>>> = Rc::default();
    let first = Rc::new(std::cell::Cell::new(true));
    let s = shared.clone();
    app.connect_activate(move |application| {
        if s.borrow().is_none() {
            let built = build(application);
            // Running in the background, as Screenlet for macOS in the menu bar: the window
            // hides when closed, and the Quick Settings toggle reaches the app over D-Bus.
            let _hold = std::mem::ManuallyDrop::new(application.hold());
            export(application, &built);
            *s.borrow_mut() = Some(built);
        }
        if !(background && first.replace(false)) {
            s.borrow().as_ref().unwrap().borrow().window.present();
        }
    });
    app.run_with_args::<&str>(&[])
}

/// The toggle's way in: io.github.davideloconte.Screenlet1 on the session bus.
fn export(application: &adw::Application, app: &Shared) {
    let Some(bus) = application.dbus_connection() else { return };
    let Some(interface) = gtk::gio::DBusNodeInfo::for_xml(BUS_XML).ok()
        .and_then(|n| n.lookup_interface("io.github.davideloconte.Screenlet1")) else { return };
    let (a, g, application) = (app.clone(), app.clone(), application.clone());
    let registered = bus.register_object(BUS_PATH, &interface)
        .method_call(move |_, _, _, _, method, _, invocation| {
            match method {
                "Connect" => connect_any(&a),
                "Disconnect" => disconnect(&a),
                "ShowWindow" => a.borrow().window.present(),
                "Quit" => {
                    disconnect(&a);
                    application.quit();
                }
                _ => {}
            }
            invocation.return_value(None);
        })
        .property(move |_, _, _, _, name| match name {
            "Active" => g.borrow().session.is_some().to_variant(),
            _ => g.borrow().status.label().to_variant(),
        })
        .build();
    if registered.is_ok() {
        app.borrow_mut().bus = Some(bus);
    }
}

/// The toggle switched on: the tablet on the cable, else the first paired one on the network, else the window.
fn connect_any(app: &Shared) {
    let plugged = app.borrow().plugged;
    if plugged && !paired::all().is_empty() {
        return over_cable(app, Then::Show);
    }
    let found = app.borrow().nearby.lock().unwrap().values().find(|t| paired::by_id(&t.id).is_some()).cloned();
    match found {
        Some(t) => show(app, Path::Network(t.address, Some(t.id.clone())), t.name),
        None => app.borrow().window.present(),
    }
}

/// Tells the toggle what changed.
fn announce(app: &Shared) {
    let a = app.borrow();
    let Some(bus) = &a.bus else { return };
    let changed = glib::VariantDict::new(None);
    changed.insert_value("Active", &a.session.is_some().to_variant());
    changed.insert_value("Title", &a.status.label().to_variant());
    let args = glib::Variant::tuple_from_iter(["io.github.davideloconte.Screenlet1".to_variant(), changed.end(),
        glib::Variant::array_from_iter_with_type(glib::VariantTy::STRING, Vec::<glib::Variant>::new())]);
    let _ = bus.emit_signal(None, BUS_PATH, "org.freedesktop.DBus.Properties", "PropertiesChanged", Some(&args));
}

fn combo(title: &str, help: &str, items: &[&str], selected: usize) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(items)).build();
    row.set_selected(selected as u32);
    if !help.is_empty() {
        row.set_tooltip_text(Some(help));
    }
    row
}

fn index<T: PartialEq>(values: &[T], value: &T) -> usize {
    values.iter().position(|v| v == value).unwrap_or(0)
}

fn build(application: &adw::Application) -> Shared {
    let settings = Settings::load();
    let window = adw::ApplicationWindow::builder().application(application).title("Screenlet").default_width(520)
        .default_height(760).hide_on_close(true).build();
    let page = adw::PreferencesPage::new();

    // Header: what is happening.
    let head = adw::PreferencesGroup::new();
    let status = gtk::Label::builder().label("No tablet yet").wrap(true).xalign(0.0).css_classes(["title-4"]).build();
    head.add(&status);
    page.add(&head);

    let devices = adw::PreferencesGroup::builder().title("Devices")
        .description("Open Screenlet on the tablet, on the same Wi-Fi as this PC or on the USB cable.").build();
    let connections = ["auto", "wifi", "cable"];
    let connection = combo("Connection", HELP_CONNECTION, &["Automatic", "Wi-Fi", "Cable"], index(&connections, &settings.connection.as_str()));
    devices.add(&connection);
    page.add(&devices);

    let display = adw::PreferencesGroup::builder().title("Display").build();
    let mirroring = combo("Show the desktop", HELP_MODE, &["Extend", "Mirror"], settings.mirroring as usize);
    let monitors: Vec<(String, String)> = crate::mutter::monitors().map(|(_, m, _)| m).unwrap_or_default().into_iter()
        .filter(|m| !m.connector.starts_with("Meta-")).map(|m| (m.connector, m.name)).collect();
    let names: Vec<&str> = monitors.iter().map(|m| m.1.as_str()).collect();
    let mirror = combo("Monitor", "", &names, monitors.iter().position(|m| m.0 == settings.mirror).unwrap_or(0));
    mirror.set_visible(settings.mirroring);
    let resolution = combo("Resolution", HELP_RESOLUTION, &["Automatic"], 0);
    resolution.set_visible(!settings.mirroring);
    let priorities = ["latency", "balanced", "clarity"];
    let priority = combo("Optimize for", HELP_PRIORITY, &["Lowest latency", "Balanced", "Sharpest"],
                         index(&priorities, &settings.mode.priority.as_str()));
    display.add(&mirroring);
    display.add(&mirror);
    display.add(&resolution);
    display.add(&priority);
    page.add(&display);

    let general = adw::PreferencesGroup::builder().title("Settings").build();
    let auto_start = adw::SwitchRow::builder().title("Show the desktop when the tablet is plugged in").active(settings.auto_start).build();
    general.add(&auto_start);
    page.add(&general);

    // Advanced: each overrides the preset and the tablet, from the next session.
    let advanced = adw::PreferencesGroup::builder().title("Advanced").description("From the next session.").build();
    let fpses = [0u32, 120, 90, 60, 30];
    let fps = combo("Refresh rate", "The tablet's: as fast as its panel refreshes (up to 120 Hz) and its decoder keeps up.",
                    &["The tablet's", "120 Hz", "90 Hz", "60 Hz", "30 Hz"], index(&fpses, &settings.mode.fps));
    let codecs = ["auto", "avc", "hevc"];
    let codec = combo("Codec", "Automatic: H.264, the quickest; HEVC for Sharpest. The tablet takes the first it decodes in hardware.",
                      &["Automatic", "H.264", "HEVC (H.265)"], index(&codecs, &settings.mode.codec.as_str()));
    let rates = [0u32, 8000, 12000, 20000, 30000, 40000, 60000];
    let kbps = combo("Bitrate", "The most the stream may use; it still goes down by itself when frames arrive late.",
                     &["Automatic", "8 Mbit/s", "12 Mbit/s", "20 Mbit/s", "30 Mbit/s", "40 Mbit/s", "60 Mbit/s"],
                     index(&rates, &settings.mode.kbps));
    let frames = [0.0, 1.0, 2.0, 4.0, 8.0];
    let vbv = combo("Largest frame", "How large one frame may be against the average: large frames take longer to send.",
                    &["Automatic", "1× the average", "2× the average", "4× the average", "8× the average"],
                    frames.iter().position(|f| *f == settings.mode.vbv_frames).unwrap_or(0));
    let packet_kinds = ["auto", "large", "small"];
    let packets = combo("Packets", "Large ones cost the tablet fewer decryptions; small ones fit one Ethernet frame each, for networks that drop IP fragments. Automatic turns to small ones if no frame arrives.",
                        &["Automatic", "Large", "Small"], index(&packet_kinds, &settings.mode.packets.as_str()));
    let periods = [0u32, 2, 5, 10, 30, 60];
    let keyframes = combo("Keyframes", "A keyframe comes whenever the tablet lost something; these come besides. Rarer ones mean fewer peaks of latency.",
                          &["Automatic", "Every 2 s", "Every 5 s", "Every 10 s", "Every 30 s", "Every 60 s"],
                          index(&periods, &settings.mode.keyframe_seconds));
    let restore = gtk::Button::builder().label("Restore defaults").valign(gtk::Align::Center).build();
    let expander = adw::ExpanderRow::builder().title("Codec, bitrate, packets").build();
    for row in [&fps, &codec, &kbps, &vbv, &packets, &keyframes] {
        expander.add_row(row);
    }
    let restore_row = adw::ActionRow::new();
    restore_row.add_suffix(&restore);
    expander.add_row(&restore_row);
    advanced.add(&expander);
    page.add(&advanced);

    let statistics = adw::PreferencesGroup::builder().title("Statistics").description(HELP_STATS).build();
    let stats = ["Capture to decoded", "Encoding and sending", "Receiving and decoding", "Frames", "Bitrate"]
        .map(|title| adw::ActionRow::builder().title(title).subtitle("–").subtitle_selectable(true).build());
    for row in &stats {
        statistics.add(row);
    }
    page.add(&statistics);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    window.set_content(Some(&toolbar));

    let app: Shared = Rc::new(RefCell::new(App {
        window: window.clone(), settings, session: None, cable: None, plugged: false, checked: Instant::now() - Duration::from_secs(9),
        nearby: nearby::browse(), events: channel(), busy: None, status, devices, rows: Vec::new(), shown: String::new(),
        stats, resolution: resolution.clone(), resolutions: Vec::new(), bus: None, announced: (false, String::new()),
    }));
    refresh_resolutions(&app);

    // Each choice saved as it is made.
    let changed = |app: &Shared, f: Box<dyn Fn(&mut Settings, u32)>| {
        let app = app.clone();
        move |row: &adw::ComboRow| {
            let mut a = app.borrow_mut();
            f(&mut a.settings, row.selected());
            a.settings.save();
        }
    };
    connection.connect_selected_notify(changed(&app, Box::new(move |s, i| s.connection = connections[i as usize].into())));
    {
        let (mirror_row, resolution_row) = (mirror.clone(), resolution.clone());
        let f = changed(&app, Box::new(move |s, i| s.mirroring = i == 1));
        mirroring.connect_selected_notify(move |row| {
            f(row);
            mirror_row.set_visible(row.selected() == 1);
            resolution_row.set_visible(row.selected() == 0);
        });
    }
    let connectors: Vec<String> = monitors.iter().map(|m| m.0.clone()).collect();
    mirror.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mirror = connectors.get(i as usize).cloned().unwrap_or_default())));
    {
        let app2 = app.clone();
        resolution.connect_selected_notify(move |row| {
            let mut a = app2.borrow_mut();
            if let Some(r) = a.resolutions.get(row.selected() as usize).cloned() {
                a.settings.mode.resolution = r;
                a.settings.save();
            }
        });
    }
    priority.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.priority = priorities[i as usize].into())));
    fps.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.fps = fpses[i as usize])));
    codec.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.codec = codecs[i as usize].into())));
    kbps.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.kbps = rates[i as usize])));
    vbv.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.vbv_frames = frames[i as usize])));
    packets.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.packets = packet_kinds[i as usize].into())));
    keyframes.connect_selected_notify(changed(&app, Box::new(move |s, i| s.mode.keyframe_seconds = periods[i as usize])));
    {
        let app2 = app.clone();
        auto_start.connect_active_notify(move |row| {
            let mut a = app2.borrow_mut();
            a.settings.auto_start = row.is_active();
            a.settings.save();
        });
    }
    restore.connect_clicked(move |_| {
        fps.set_selected(0);
        codec.set_selected(0);
        kbps.set_selected(0);
        vbv.set_selected(0);
        packets.set_selected(0);
        keyframes.set_selected(0);
    });

    let app2 = app.clone();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        tick(&app2);
        glib::ControlFlow::Continue
    });
    app
}

/// The resolutions for the tablet last seen: Automatic, native, then smaller at its proportions.
fn refresh_resolutions(app: &Shared) {
    let mut a = app.borrow_mut();
    let screen = a.settings.tablet_screen;
    let mut names = vec![screen.and_then(sizes::models).map_or("Automatic".to_string(), |m| format!("Automatic ({m})"))];
    let mut values = vec!["auto".to_string()];
    match screen {
        Some(screen) => for (i, size) in sizes::all(screen).into_iter().enumerate() {
            names.push(if i == 0 { format!("{} × {} (native)", size.0, size.1) } else { format!("{} × {}", size.0, size.1) });
            values.push(if i == 0 { "tablet".into() } else { format!("{}x{}", size.0, size.1) });
        },
        None => {
            names.push("Native".into());
            values.push("tablet".into());
        }
    }
    let selected = index(&values, &a.settings.mode.resolution);
    let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
    a.resolutions = values;
    let row = a.resolution.clone();
    drop(a);
    row.set_model(Some(&model));
    row.set_selected(selected as u32);
}

/// An action on a tablet: a row's button.
struct Action {
    device: String,
    detail: &'static str,
    label: &'static str,
    primary: bool,
    run: Box<dyn Fn(&Shared)>,
}

fn actions(app: &Shared) -> Vec<Action> {
    let a = app.borrow();
    if let Some((_, name)) = &a.session {
        return vec![Action { device: name.clone(), detail: "Showing the desktop", label: "Disconnect", primary: false,
                             run: Box::new(disconnect) }];
    }
    let paired = paired::all();
    let by_cable = a.plugged && a.settings.connection != "wifi";
    let by_wifi = a.settings.connection == "wifi" || (a.settings.connection == "auto" && !a.plugged);
    let mut found = Vec::new();
    let mut tablets: Vec<Tablet> = a.nearby.lock().unwrap().values().cloned().collect();
    tablets.sort_by_key(|t| t.name.to_lowercase());
    let mut seen = Vec::new();
    for t in tablets {
        if paired::by_id(&t.id).is_some() {
            seen.push(t.id.clone());
            if by_wifi {
                let path = Path::Network(t.address, Some(t.id.clone()));
                let name = t.name.clone();
                found.push(Action { device: t.name, detail: "Wi-Fi", label: "Show the desktop", primary: true,
                                    run: Box::new(move |app| show(app, path.clone(), name.clone())) });
            }
        } else if a.settings.connection != "cable" {
            let path = Path::Network(t.address, None);
            found.push(Action { device: t.name, detail: "Wi-Fi", label: "Pair…", primary: false,
                                run: Box::new(move |app| pair(app, path.clone())) });
        }
    }
    if by_cable {
        if !paired.is_empty() {
            found.push(Action { device: "Tablet on the cable".into(), detail: "USB-C cable", label: "Show the desktop", primary: true,
                                run: Box::new(|app| over_cable(app, Then::Show)) });
        }
        found.push(Action { device: "Tablet on the cable".into(), detail: "USB-C cable", label: "Pair…", primary: paired.is_empty(),
                            run: Box::new(|app| over_cable(app, Then::Pair)) });
    }
    // A paired tablet nowhere to be seen, whose own Wi-Fi Direct group this PC knows.
    if by_wifi {
        for (fp, entry) in &paired {
            let id: String = fp.chars().take(16).collect();
            let (Some(ssid), Some(passphrase)) = (entry["p2p"]["ssid"].as_str(), entry["p2p"]["passphrase"].as_str()) else { continue };
            if seen.contains(&id) {
                continue;
            }
            let name = entry["name"].as_str().unwrap_or("tablet").to_string();
            let (ssid, passphrase) = (ssid.to_string(), passphrase.to_string());
            let at = entry["p2p"]["at"].as_str().unwrap_or("192.168.49.1").to_string();
            found.push(Action { device: name.clone(), detail: "Wi-Fi Direct", label: "Show the desktop", primary: true,
                                run: Box::new(move |app| direct(app, &ssid, &passphrase, &at, &id, &name)) });
        }
    }
    found
}

fn disconnect(app: &Shared) {
    if let Some((session, _)) = app.borrow_mut().session.take() {
        session.stop();
    }
    app.borrow().status.set_label("Disconnected");
}

fn show(app: &Shared, path: Path, name: String) {
    let mut a = app.borrow_mut();
    if a.session.is_some() {
        return;
    }
    let session = Session::start(path, a.settings.session_mode(), false);
    a.status.set_label(&format!("Connecting to {name}…"));
    a.session = Some((session, name));
}

fn pair(app: &Shared, path: Path) {
    let tx = app.borrow().events.0.clone();
    app.borrow_mut().busy = Some("Pairing…".into());
    std::thread::spawn(move || {
        let asker = Mutex::new(tx.clone());
        let ask = move |name: &str, code: &str| {
            let (answer, wait) = channel();
            let _ = asker.lock().unwrap().send(Event::Ask(name.into(), code.into(), answer));
            wait.recv().unwrap_or(false)
        };
        let result = session::pair(&path, &ask).map_err(|e| e.to_string());
        let _ = tx.send(Event::Paired(result));
    });
}

fn over_cable(app: &Shared, then: Then) {
    if let Some(cable) = app.borrow().cable.clone().filter(|c| c.alive()) {
        return run_on_cable(app, cable, then);
    }
    let tx = app.borrow().events.0.clone();
    app.borrow_mut().busy = Some("Opening the link over the cable… (allow Screenlet on the tablet)".into());
    std::thread::spawn(move || {
        let _ = tx.send(Event::Cable(Cable::open().map_err(|e| e.to_string()), then));
    });
}

fn run_on_cable(app: &Shared, cable: Arc<Cable>, then: Then) {
    match then {
        Then::Show => show(app, Path::Cable(cable), "the tablet on the cable".into()),
        Then::Pair => pair(app, Path::Cable(cable)),
    }
}

/// Joins the tablet's own Wi-Fi Direct group as a Wi-Fi network (NetworkManager), then shows the desktop there.
fn direct(app: &Shared, ssid: &str, passphrase: &str, at: &str, id: &str, name: &str) {
    app.borrow().status.set_label(&format!("Joining {name}'s Wi-Fi Direct network…"));
    let joined = std::process::Command::new("nmcli").args(["device", "wifi", "connect", ssid, "password", passphrase])
        .status().map(|s| s.success()).unwrap_or(false);
    let address: Option<SocketAddr> = format!("{at}:{}", crate::protocol::PORT).parse().ok();
    match (joined, address) {
        (true, Some(address)) => show(app, Path::Network(address, Some(id.into())), name.into()),
        _ => alert(app, &format!("Could not join {name}'s Wi-Fi Direct network"), "Is Screenlet open on the tablet, with Wi-Fi on?"),
    }
}

fn alert(app: &Shared, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("ok", "OK");
    dialog.present(Some(&app.borrow().window));
}

fn tick(app: &Shared) {
    // What the threads said.
    loop {
        let event = app.borrow().events.1.try_recv();
        let Ok(event) = event else { break };
        match event {
            Event::Ask(name, code, answer) => {
                let dialog = adw::AlertDialog::new(Some(&format!("Pair with {name}?")),
                    Some(&format!("Pair only if {name} shows the same code:\n\n{} {}", &code[..3], &code[3..])));
                dialog.add_response("no", "Cancel");
                dialog.add_response("pair", "Pair");
                dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
                let answer = RefCell::new(Some(answer));
                dialog.connect_response(None, move |_, response| {
                    if let Some(answer) = answer.borrow_mut().take() {
                        let _ = answer.send(response == "pair");
                    }
                });
                dialog.present(Some(&app.borrow().window));
            }
            Event::Paired(result) => {
                app.borrow_mut().busy = None;
                match result {
                    Ok(name) => app.borrow().status.set_label(&format!("Paired with {name}")),
                    Err(e) => alert(app, "Not paired", &e),
                }
            }
            Event::Cable(result, then) => {
                app.borrow_mut().busy = None;
                match result {
                    Ok(cable) => {
                        app.borrow_mut().cable = Some(cable.clone());
                        run_on_cable(app, cable, then);
                    }
                    Err(e) => alert(app, "Screenlet could not use the cable", &e),
                }
            }
        }
    }
    // The cable, looked at every second: there is no notification for it here.
    if app.borrow().checked.elapsed() >= Duration::from_secs(1) {
        let now = app.borrow().cable.as_ref().is_some_and(|c| c.alive()) || cable::present();
        let was = std::mem::replace(&mut app.borrow_mut().plugged, now);
        app.borrow_mut().checked = Instant::now();
        let a = app.borrow();
        let start = now && !was && a.settings.auto_start && a.settings.connection != "wifi" && a.session.is_none()
            && a.busy.is_none() && !paired::all().is_empty();
        drop(a);
        if start {
            over_cable(app, Then::Show); // plugged in: the desktop goes to it by itself
        }
    }
    // The session.
    let (ended, report, state, screen) = {
        let a = app.borrow();
        match &a.session {
            Some((s, _)) => {
                let st = s.status.lock().unwrap();
                (st.ended.clone(), st.report.clone(), Some(st.state.clone()), st.screen)
            }
            None => (None, None, None, None),
        }
    };
    if let Some(screen) = screen.filter(|s| app.borrow().settings.tablet_screen != Some(*s)) {
        app.borrow_mut().settings.tablet_screen = Some(screen);
        app.borrow().settings.save();
        refresh_resolutions(app);
    }
    if let Some(ended) = ended {
        let (_, name) = app.borrow_mut().session.take().unwrap();
        match ended {
            Some(error) => {
                app.borrow().status.set_label(&format!("Could not show the desktop on {name}"));
                alert(app, &format!("The desktop on {name} ended"), &error);
            }
            None => app.borrow().status.set_label(&format!("Ended on {name}")),
        }
    } else if let Some(state) = state {
        app.borrow().status.set_label(&state);
    } else if let Some(busy) = app.borrow().busy.clone() {
        app.borrow().status.set_label(&busy);
    } else if app.borrow().status.label().starts_with("No tablet") || app.borrow().status.label() == "A tablet is ready" {
        let any = !app.borrow().nearby.lock().unwrap().is_empty() || app.borrow().plugged;
        app.borrow().status.set_label(if any { "A tablet is ready" } else { "No tablet with Screenlet open" });
    }
    show_report(app, report);
    show_devices(app);
    let now = (app.borrow().session.is_some(), app.borrow().status.label().to_string());
    if now != app.borrow().announced {
        app.borrow_mut().announced = now;
        announce(app);
    }
}

fn show_report(app: &Shared, report: Option<Report>) {
    let a = app.borrow();
    let ms = |v: Option<f64>| v.map_or("–".to_string(), |v| format!("{v:.1} ms"));
    let lines = match report {
        Some(r) => [
            format!("{:.1} ms (90%: {:.1})", r.latency, r.latency90),
            format!("{} (encoded at {})", ms(r.sending), ms(r.encoded)),
            format!("{:.1} ms (spread {}, decoder wait {}, decode {})", r.decoding, ms(r.stages[2]), ms(r.stages[3]), ms(r.stages[4])),
            format!("{} fps, {} lost{}", r.fps, r.lost, if r.skipped > 0 { format!(", {} not encoded", r.skipped) } else { String::new() }),
            format!("{:.1} of {:.1} Mbit/s", r.kbps as f64 / 1000.0, r.cap as f64 / 1000.0),
        ],
        None => std::array::from_fn(|_| "Shown while the desktop is on the tablet".to_string()),
    };
    for (row, line) in a.stats.iter().zip(lines) {
        if row.subtitle().as_deref() != Some(line.as_str()) {
            row.set_subtitle(&line);
        }
    }
}

fn show_devices(app: &Shared) {
    let actions = actions(app);
    let signature: String = actions.iter().map(|a| format!("{}|{}|{};", a.device, a.detail, a.label)).collect();
    if signature == app.borrow().shown {
        return;
    }
    let mut a = app.borrow_mut();
    a.shown = signature;
    let devices = a.devices.clone();
    for row in a.rows.drain(..) {
        devices.remove(&row);
    }
    let mut by_device: Vec<(String, &'static str, Vec<Action>)> = Vec::new();
    for action in actions {
        match by_device.iter_mut().find(|d| d.0 == action.device) {
            Some(d) => d.2.push(action),
            None => by_device.push((action.device.clone(), action.detail, vec![action])),
        }
    }
    for (device, detail, actions) in by_device {
        let row = adw::ActionRow::builder().title(&device).subtitle(detail).build();
        for action in actions {
            let button = gtk::Button::builder().label(action.label).valign(gtk::Align::Center).build();
            if action.primary {
                button.add_css_class("suggested-action");
            }
            let app = app.clone();
            let run = action.run;
            button.connect_clicked(move |_| run(&app));
            row.add_suffix(&button);
        }
        a.devices.add(&row);
        a.rows.push(row);
    }
}
