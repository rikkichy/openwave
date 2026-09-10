//! External, non-installed smoke tooling. Talks only to public D-Bus/AT-SPI and
//! the private server's command-line clients; never imports the app controller.
use glib::variant::ToVariant;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    error::Error,
    fs,
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;
const APP: &str = "com.github.openwave";
const APP_PATH: &str = "/com/github/openwave";

fn require_sandbox() -> Result {
    if std::env::var("OPENWAVE_SMOKE_SANDBOX").as_deref() != Ok("1")
        || std::env::var("HOME").as_deref() != Ok("/work/home")
        || Path::new("/dev/bus/usb").exists()
        || Path::new("/dev/snd").exists()
        || (Path::new("/proc/asound").exists() && fs::read_dir("/proc/asound")?.next().is_some())
        || std::env::var("XDG_RUNTIME_DIR").as_deref() != Ok("/work/run")
    {
        return Err(
            "Smoke driver requires the packaging/smoke-install.sh device-free sandbox".into(),
        );
    }
    Ok(())
}
fn session() -> Result<gio::DBusConnection> {
    Ok(gio::bus_get_sync(
        gio::BusType::Session,
        gio::Cancellable::NONE,
    )?)
}
fn call(
    bus: &gio::DBusConnection,
    name: &str,
    path: &str,
    iface: &str,
    method: &str,
    args: Option<&glib::Variant>,
) -> Result<glib::Variant> {
    Ok(bus.call_sync(
        Some(name),
        path,
        iface,
        method,
        args,
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        5000,
        gio::Cancellable::NONE,
    )?)
}
fn activate(path: &str, action: &str, parameter: Option<&str>) -> Result {
    let params = parameter
        .map(|text| glib::Variant::parse(None, text))
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    call(
        &session()?,
        APP,
        path,
        "org.gtk.Actions",
        "Activate",
        Some(&(action, params, HashMap::<String, glib::Variant>::new()).to_variant()),
    )?;
    Ok(())
}
fn state(action: &str) -> Result<Value> {
    if !matches!(action, "snapshot" | "scenes" | "levels") {
        return Err("Expected a JSON read action".into());
    }
    // Read actions refresh their public state when activated.
    activate(APP_PATH, action, None)?;
    let reply = call(
        &session()?,
        APP,
        APP_PATH,
        "org.gtk.Actions",
        "Describe",
        Some(&(action,).to_variant()),
    )?;
    if reply.type_().as_str() != "((bgav))" {
        return Err(format!("Invalid org.gtk.Actions.Describe reply: {}", reply.type_()).into());
    }
    let states = reply.child_value(0).child_value(2);
    if states.n_children() != 1 {
        return Err(format!("Action {action} must have one state").into());
    }
    let boxed = states.child_value(0);
    let value = boxed.as_variant().ok_or("Action state is not a variant")?;
    let text = value.str().ok_or("Action state is not JSON text")?;
    Ok(serde_json::from_str(text)?)
}
fn command(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("timeout")
        .args(["12", program])
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}
fn graph() -> Result<Vec<Value>> {
    Ok(serde_json::from_str(&command("pw-dump", &[])?)?)
}
fn node<'a>(graph: &'a [Value], name: &str) -> Result<&'a Value> {
    let nodes: Vec<_> = graph
        .iter()
        .filter(|v| {
            v["type"] == "PipeWire:Interface:Node" && v["info"]["props"]["node.name"] == name
        })
        .collect();
    if nodes.len() != 1 {
        return Err(format!("Expected exactly one node {name}, found {}", nodes.len()).into());
    }
    Ok(nodes[0])
}
fn identity(graph: &[Value], name: &str) -> Result<Value> {
    let core = graph
        .iter()
        .find(|v| v["type"] == "PipeWire:Interface:Core")
        .ok_or("No private Core identity")?;
    let n = node(graph, name)?;
    let serial = &n["info"]["props"]["object.serial"];
    if core["info"]["cookie"].is_null() || serial.is_null() {
        return Err("Missing Core cookie/node serial".into());
    }
    Ok(json!({"cookie":core["info"]["cookie"],"serial":serial}))
}
fn wait_for<T>(label: &str, mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if Instant::now() >= deadline => {
                return Err(format!("Timed out waiting for {label}: {error}").into());
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}
fn fixtures(directory: &Path) -> Result {
    fs::create_dir_all(directory)?;
    let source = |id: &str, kind: &str, name: &str, binding: &str, group: &str, muted: bool| {
        json!({
            "id":id,"kind":kind,"name":name,"icon_name":if kind == "app" {"audio-x-generic-symbolic"} else {"audio-input-microphone-symbolic"},
            "match_app_names":if kind == "app" {vec![binding]} else {vec![]},"node_name":if kind == "device" {binding} else {""},
            "level":1.0,"muted":muted,"protected":false,"group":group
        })
    };
    let sources = json!({
        "music":source("music","app","Music","Fixture Music","",false),
        "voice":source("voice","app","Voice","Fixture Voice","",false),
        "mic_a":source("mic_a","device","Mic A","fixture_mic_a","Mics",false),
        "mic_b":source("mic_b","device","Mic B","fixture_mic_b","Mics",true)
    });
    let mut mixes = serde_json::Map::new();
    for (id, name, description) in [
        ("personal", "Personal", "Your monitor mix"),
        ("chat", "Chat", "Voice chat mix"),
        ("record", "Record", "Recording mix"),
    ] {
        mixes.insert(id.into(),json!({"id":id,"name":name,"description":description,"subtitle":description,"icon_name":"audio-card-symbolic","sink":format!("openwave_{id}_mix")}));
    }
    for (file, contents) in [
        ("sources.json", sources),
        ("mixdefs.json", Value::Object(mixes)),
        (
            "mixes.json",
            json!({"music.personal":{"volume":0.4,"muted":false},"outputs":{"personal":"none","chat":"missing_output","record":"none"},"volumes":{"personal":{"volume":1.0,"muted":false},"chat":{"volume":1.0,"muted":false},"record":{"volume":1.0,"muted":false}}}),
        ),
        (
            "ui-state.json",
            json!({"width":1280,"height":720,"maximized":false,"offered_capture_nodes":["fixture_mic_a","fixture_mic_b"],"gain_locked":false,"tray_icon_color":"white"}),
        ),
        ("scenes.json", json!({"scenes":{}})),
    ] {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join(file))?;
        file.write_all(&serde_json::to_vec_pretty(&contents)?)?;
    }
    Ok(())
}
fn tone() -> Result {
    // 48 frames is exactly one 1 kHz cycle at 48 kHz; no phase drift or clipping.
    let mut block = Vec::with_capacity(480 * 8);
    for i in 0..480 {
        let sample = (0.25 * (std::f64::consts::TAU * (i % 48) as f64 / 48.0).sin()) as f32;
        block.extend_from_slice(&sample.to_le_bytes());
        block.extend_from_slice(&sample.to_le_bytes());
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    loop {
        if let Err(error) = out.write_all(&block) {
            return if error.kind() == io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(error.into())
            };
        }
    }
}
fn ports(graph: &[Value], id: &Value, direction: &str) -> Vec<(String, u64)> {
    let mut result: Vec<_> = graph
        .iter()
        .filter(|v| {
            v["type"] == "PipeWire:Interface:Port"
                && v["info"]["props"]["node.id"].to_string().trim_matches('"')
                    == id.to_string().trim_matches('"')
                && v["info"]["direction"] == direction
        })
        .filter_map(|v| {
            Some((
                v["info"]["props"]["audio.channel"].as_str()?.to_owned(),
                v["id"].as_u64()?,
            ))
        })
        .collect();
    result.sort();
    result
}
fn link_fixture(source: &str, target: &str) -> Result {
    // Fixture-only links. Never repair an OpenWave-owned route here.
    if !source.starts_with("fixture_") && target != "smoke_recorder" {
        return Err("Only fixture/measurement links may be created by smoke tooling".into());
    }
    let g = graph()?;
    let outputs = ports(&g, &node(&g, source)?["id"], "output");
    let inputs = ports(&g, &node(&g, target)?["id"], "input");
    if outputs.len() != 2 || inputs.len() != 2 {
        return Err("Fixture stream does not have stereo ports yet".into());
    }
    for ((a, output), (b, input)) in outputs.iter().zip(&inputs) {
        if a != b {
            return Err("Mismatched measurement channel map".into());
        }
        let already = g.iter().any(|v| {
            v["type"] == "PipeWire:Interface:Link"
                && v["info"]["output-port-id"] == *output
                && v["info"]["input-port-id"] == *input
        });
        if !already {
            command("pw-link", &[&output.to_string(), &input.to_string()])?;
        }
    }
    Ok(())
}
fn record(source: &str, file: &str, expected: f64) -> Result {
    if !expected.is_finite() || expected < 0.0 {
        return Err("Invalid expected PCM peak".into());
    }
    // File is fresh, bounded by a ten-second timeout. Discard two seconds for
    // link establishment and old buffers, then require two seconds of samples.
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)?;
    let mut child = Command::new("timeout")
        .args([
            "--signal=INT",
            "--kill-after=2",
            "10",
            "pw-cat",
            // Stdout is headerless PCM on PipeWire 1.0 as well as newer releases.
            "--record",
            "--format=f32",
            "--rate=48000",
            "--channels=2",
            "--target=0",
            "--properties={ node.name = smoke_recorder node.autoconnect = false }",
            "-",
        ])
        .stdout(Stdio::from(output))
        .spawn()?;
    let outcome = (|| -> Result {
        wait_for("measurement stereo links", || {
            link_fixture(source, "smoke_recorder")
        })?;
        thread::sleep(Duration::from_secs(5));
        let bytes = fs::read(file)?;
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        if bytes.len() % 8 != 0 || samples.len() < 4 * 48_000 * 2 {
            return Err(format!("Insufficient complete stereo PCM: {} bytes", bytes.len()).into());
        }
        let measured = &samples[2 * 48_000 * 2..4 * 48_000 * 2];
        if measured.iter().any(|s| !s.is_finite()) {
            return Err("Nonfinite published PCM".into());
        }
        let left = measured
            .iter()
            .step_by(2)
            .fold(0.0_f32, |a, b| a.max(b.abs()));
        let right = measured
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0.0_f32, |a, b| a.max(b.abs()));
        let tolerance = (expected * 0.02).max(0.00001);
        for peak in [left, right] {
            if (f64::from(peak) - expected).abs() >= tolerance {
                return Err(
                    format!("PCM {file}: peak {peak}, expected {expected} ± {tolerance}").into(),
                );
            }
        }
        fs::write(
            format!("{file}.json"),
            serde_json::to_vec_pretty(
                &json!({"expected":expected,"left_peak":left,"right_peak":right,"tolerance":tolerance,"recorded_frames":samples.len()/2,"checked_frames":96000,"discarded_frames":96000}),
            )?,
        )?;
        println!("PCM {file}: L={left:.6}, R={right:.6}, expected={expected:.6}");
        Ok(())
    })();
    // Successful recording ends at our timeout's deadline. Killing pw-cat
    // early is ambiguous: its intentional SIGINT shutdown also returns 1.
    // An early measurement failure cancels only the captured timeout child.
    if outcome.is_err() {
        if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
        }
    }
    let status = child.wait()?;
    if outcome.is_ok() && status.code() != Some(124) {
        return Err(format!("PCM recorder failed: {status}").into());
    }
    outcome
}
fn route_assert() -> Result {
    let g = graph()?;
    if g.iter().any(|v| {
        v["info"]["props"]["node.name"]
            .as_str()
            .is_some_and(|s| s.starts_with("openwave_loop_output_chat"))
    }) {
        return Err("Missing explicit Chat output gained an alternate monitor route".into());
    }
    node(&g, "openwave_capture_chat")?;
    let inputs: Vec<Value> = serde_json::from_str(&command(
        "pactl",
        &["--format=json", "list", "sink-inputs"],
    )?)?;
    let sinks: Vec<Value> =
        serde_json::from_str(&command("pactl", &["--format=json", "list", "sinks"])?)?;
    let intake = sinks
        .iter()
        .find(|s| s["name"] == "openwave_src_music")
        .ok_or("Missing persistent Music intake")?;
    let stream = inputs
        .iter()
        .find(|s| s["properties"]["application.name"] == "Fixture Music")
        .ok_or("Missing real Fixture Music Pulse stream")?;
    if stream["sink"] != intake["index"] {
        return Err("Music stream was not moved exclusively into its intake".into());
    }
    let stream_serial = &stream["properties"]["object.serial"];
    let stream_node = g
        .iter()
        .find(|n| {
            n["type"] == "PipeWire:Interface:Node"
                && n["info"]["props"]["object.serial"]
                    .to_string()
                    .trim_matches('"')
                    == stream_serial.to_string().trim_matches('"')
        })
        .ok_or("Missing native stream correlation")?;
    let links: Vec<_> = g
        .iter()
        .filter(|v| {
            v["type"] == "PipeWire:Interface:Link"
                && v["info"]["output-node-id"] == stream_node["id"]
        })
        .collect();
    let intake_node = node(&g, "openwave_src_music")?;
    if links.len() != 2
        || links
            .iter()
            .any(|v| v["info"]["input-node-id"] != intake_node["id"])
    {
        return Err("Music stream still bypasses intake or lacks stereo links".into());
    }
    Ok(())
}
fn break_link(file: &Path) -> Result {
    let g = graph()?;
    let route = node(&g, "openwave_loop_5_music_personal")?;
    if route["info"]["props"]["openwave.owner"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Err("Refusing to break unowned route".into());
    }
    let link = g
        .iter()
        .find(|v| {
            v["type"] == "PipeWire:Interface:Link" && v["info"]["output-node-id"] == route["id"]
        })
        .ok_or("No live owned cell link to remove")?;
    let endpoints = json!({"output":link["info"]["output-port-id"],"input":link["info"]["input-port-id"],"publication":identity(&g,"openwave_capture_personal")?});
    fs::write(file, serde_json::to_vec_pretty(&endpoints)?)?;
    command(
        "pw-link",
        &[
            "--disconnect",
            &endpoints["output"].to_string(),
            &endpoints["input"].to_string(),
        ],
    )?;
    wait_for("normal reconciliation to repair lost route link", || {
        let g = graph()?;
        if identity(&g, "openwave_capture_personal")? != endpoints["publication"] {
            return Err("Publication changed during link repair".into());
        }
        if !g.iter().any(|v| {
            v["type"] == "PipeWire:Interface:Link"
                && v["info"]["output-port-id"] == endpoints["output"]
                && v["info"]["input-port-id"] == endpoints["input"]
        }) {
            return Err("Lost link not repaired".into());
        }
        Ok(())
    })
}
const WATCHER: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
<signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
<signal name="StatusNotifierHostRegistered"/><signal name="StatusNotifierHostUnregistered"/>
</interface></node>"#;
fn watcher(item_file: &str, ready_file: &str) -> Result {
    let bus = session()?;
    let xml = gio::DBusNodeInfo::for_xml(WATCHER)?;
    let iface = xml
        .lookup_interface("org.kde.StatusNotifierWatcher")
        .ok_or("Invalid watcher XML")?;
    let item_file = item_file.to_owned();
    let items = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let method_items = items.clone();
    let _registration = bus
        .register_object("/StatusNotifierWatcher", &iface)
        .method_call(
            move |connection, sender, _, _, method, parameters, invocation| match method {
                "RegisterStatusNotifierItem" => {
                    let Some((service,)) = parameters.get::<(String,)>() else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "Expected service",
                        );
                        return;
                    };
                    let (destination, path) = if service.starts_with('/') {
                        (sender.unwrap_or("").to_owned(), service)
                    } else {
                        (service, "/StatusNotifierItem".to_owned())
                    };
                    if destination.is_empty() {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "Missing sender",
                        );
                        return;
                    }
                    let address = format!("{destination}{path}");
                    let contents =
                        match serde_json::to_vec(&json!({"destination":destination,"path":path})) {
                            Ok(contents) => contents,
                            Err(error) => {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.Failed",
                                    &error.to_string(),
                                );
                                return;
                            }
                        };
                    if let Err(error) = fs::write(&item_file, contents) {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.Failed",
                            &error.to_string(),
                        );
                        return;
                    }
                    method_items.borrow_mut().push(address.clone());
                    invocation.return_value(Some(&().to_variant()));
                    let _ = connection.emit_signal(
                        None,
                        "/StatusNotifierWatcher",
                        "org.kde.StatusNotifierWatcher",
                        "StatusNotifierItemRegistered",
                        Some(&(address,).to_variant()),
                    );
                }
                "RegisterStatusNotifierHost" => invocation.return_value(Some(&().to_variant())),
                _ => invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "Unknown watcher method",
                ),
            },
        )
        .property(move |_, _, _, _, property| match property {
            "RegisteredStatusNotifierItems" => items.borrow().to_variant(),
            "IsStatusNotifierHostRegistered" => true.to_variant(),
            "ProtocolVersion" => 0_i32.to_variant(),
            _ => ().to_variant(),
        })
        .build()?;
    let result = call(
        &bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&("org.kde.StatusNotifierWatcher", 4_u32).to_variant()),
    )?;
    if result.get::<(u32,)>() != Some((1,)) {
        return Err("Private watcher name already owned".into());
    }
    fs::write(ready_file, "ready\n")?;
    glib::MainLoop::new(None, false).run();
    Ok(())
}
fn tray(item_file: &str, operation: &str) -> Result {
    let item: Value = serde_json::from_slice(&fs::read(item_file)?)?;
    let destination = item["destination"]
        .as_str()
        .ok_or("Missing tray bus owner")?;
    let path = item["path"].as_str().ok_or("Missing tray path")?;
    let bus = session()?;
    match operation {
        "open" => {
            call(
                &bus,
                destination,
                path,
                "org.kde.StatusNotifierItem",
                "Activate",
                Some(&(0_i32, 0_i32).to_variant()),
            )?;
        }
        "quit" => {
            call(
                &bus,
                destination,
                "/MenuBar",
                "com.canonical.dbusmenu",
                "Event",
                Some(&(4_i32, "clicked", 0_i32.to_variant(), 0_u32).to_variant()),
            )?;
        }
        "assert-disconnected" => {
            let value = call(
                &bus,
                destination,
                "/MenuBar",
                "com.canonical.dbusmenu",
                "GetProperty",
                Some(&(2_i32, "enabled").to_variant()),
            )?;
            if value
                .child_value(0)
                .as_variant()
                .and_then(|v| v.get::<bool>())
                != Some(false)
            {
                return Err("Disconnected tray mute is enabled".into());
            }
        }
        _ => return Err("Unknown tray operation".into()),
    }
    Ok(())
}
fn accessibility_bus() -> Result<gio::DBusConnection> {
    let address = call(
        &session()?,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
        "GetAddress",
        None,
    )?
    .get::<(String,)>()
    .ok_or("Invalid AT-SPI address")?
    .0;
    Ok(gio::DBusConnection::for_address_sync(
        &address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        gio::Cancellable::NONE,
    )?)
}
fn property(
    bus: &gio::DBusConnection,
    destination: &str,
    path: &str,
    interface: &str,
    name: &str,
) -> Result<glib::Variant> {
    call(
        bus,
        destination,
        path,
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&(interface, name).to_variant()),
    )?
    .child_value(0)
    .as_variant()
    .ok_or_else(|| "Invalid property variant".into())
}
fn accessible_find(
    bus: &gio::DBusConnection,
    name: &str,
    dump: bool,
    required_role: Option<&str>,
) -> Result<(String, String)> {
    let mut queue = std::collections::VecDeque::from([(
        "org.a11y.atspi.Registry".to_owned(),
        "/org/a11y/atspi/accessible/root".to_owned(),
        0_usize,
    )]);
    let mut found = Vec::new();
    let mut visited = std::collections::HashSet::new();
    while let Some((destination, path, depth)) = queue.pop_front() {
        if depth > 40 || !visited.insert((destination.clone(), path.clone())) {
            continue;
        }
        if visited.len() > 5000 {
            return Err("AT-SPI tree exceeds smoke bound".into());
        }
        let label = property(
            bus,
            &destination,
            &path,
            "org.a11y.atspi.Accessible",
            "Name",
        )
        .ok()
        .and_then(|v| v.get::<String>())
        .unwrap_or_default();
        if dump {
            let role = call(
                bus,
                &destination,
                &path,
                "org.a11y.atspi.Accessible",
                "GetRoleName",
                None,
            )?
            .get::<(String,)>()
            .ok_or("Invalid accessible role")?
            .0;
            println!(
                "{}{} {} [{role}] {}",
                " ".repeat(depth),
                destination,
                path,
                label
            );
        }
        if label == name && !dump {
            // Match exposed widgets. Prefer a named control over its text
            // child, but allow visible text to locate an otherwise unnamed row.
            let state = call(
                bus,
                &destination,
                &path,
                "org.a11y.atspi.Accessible",
                "GetState",
                None,
            )?
            .get::<(Vec<u32>,)>()
            .ok_or("Invalid accessible state")?
            .0;
            let exposed = (1_u32 << 25) | (1_u32 << 30); // SHOWING | VISIBLE
            let role = call(
                bus,
                &destination,
                &path,
                "org.a11y.atspi.Accessible",
                "GetRoleName",
                None,
            )?
            .get::<(String,)>()
            .ok_or("Invalid accessible role")?
            .0;
            if state.first().is_some_and(|bits| bits & exposed == exposed)
                && required_role.is_none_or(|required| role == required)
            {
                found.push((
                    destination.clone(),
                    path.clone(),
                    role == "label" || role == "static",
                ));
            }
        }
        let children = match call(
            bus,
            &destination,
            &path,
            "org.a11y.atspi.Accessible",
            "GetChildren",
            None,
        ) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for child in 0..children.child_value(0).n_children() {
            let item = children.child_value(0).child_value(child);
            let Some(destination) = item.child_value(0).get::<String>() else {
                continue;
            };
            let path = item.child_value(1);
            let Some(path) = path.str() else { continue };
            if path != "/org/a11y/atspi/null" {
                queue.push_back((destination, path.to_owned(), depth + 1));
            }
        }
    }
    if dump {
        return Ok((String::new(), String::new()));
    }
    if found.iter().any(|(_, _, text)| !*text) {
        found.retain(|(_, _, text)| !*text);
    }
    if found.len() != 1 {
        return Err(format!(
            "Expected one accessible named {name:?}, found {}",
            found.len()
        )
        .into());
    }
    let (destination, path, _) = found.remove(0);
    Ok((destination, path))
}
fn ui(operation: &str, name: &str, value: Option<&str>) -> Result {
    let bus = accessibility_bus()?;
    let role = if operation == "click" { value } else { None };
    let (destination, path) = wait_for("real accessible widget", || {
        accessible_find(&bus, name, operation == "dump", role)
    })?;
    match operation {
        "dump" => {}
        "click" => {
            // Popover surfaces need not expose global screen coordinates.
            // Use the real Action or Selection interface of the owning widget.
            let (mut destination, mut path) = (destination, path);
            let mut activated = false;
            for _ in 0..8 {
                let parent = property(
                    &bus,
                    &destination,
                    &path,
                    "org.a11y.atspi.Accessible",
                    "Parent",
                )?;
                let parent_destination = parent
                    .child_value(0)
                    .get::<String>()
                    .ok_or("Invalid accessible parent owner")?;
                let parent_path = parent.child_value(1);
                let parent_path = parent_path
                    .str()
                    .ok_or("Invalid accessible parent path")?
                    .to_owned();
                let has_parent =
                    !parent_destination.is_empty() && parent_path != "/org/a11y/atspi/null";
                if has_parent {
                    let interfaces = call(
                        &bus,
                        &parent_destination,
                        &parent_path,
                        "org.a11y.atspi.Accessible",
                        "GetInterfaces",
                        None,
                    )?
                    .get::<(Vec<String>,)>()
                    .ok_or("Invalid accessible interfaces")?
                    .0;
                    if interfaces
                        .iter()
                        .any(|interface| interface == "org.a11y.atspi.Selection")
                    {
                        let index = call(
                            &bus,
                            &destination,
                            &path,
                            "org.a11y.atspi.Accessible",
                            "GetIndexInParent",
                            None,
                        )?
                        .get::<(i32,)>()
                        .ok_or("Invalid accessible child index")?
                        .0;
                        activated = call(
                            &bus,
                            &parent_destination,
                            &parent_path,
                            "org.a11y.atspi.Selection",
                            "SelectChild",
                            Some(&(index,).to_variant()),
                        )?
                        .get::<(bool,)>()
                        .ok_or("Invalid accessible selection result")?
                        .0;
                        break;
                    }
                }
                let interfaces = call(
                    &bus,
                    &destination,
                    &path,
                    "org.a11y.atspi.Accessible",
                    "GetInterfaces",
                    None,
                )?
                .get::<(Vec<String>,)>()
                .ok_or("Invalid accessible interfaces")?
                .0;
                if interfaces
                    .iter()
                    .any(|interface| interface == "org.a11y.atspi.Action")
                {
                    activated = call(
                        &bus,
                        &destination,
                        &path,
                        "org.a11y.atspi.Action",
                        "DoAction",
                        Some(&(0_i32,).to_variant()),
                    )?
                    .get::<(bool,)>()
                    .ok_or("Invalid accessible action result")?
                    .0;
                    break;
                }
                if !has_parent {
                    break;
                }
                destination = parent_destination;
                path = parent_path;
            }
            if !activated {
                return Err(
                    format!("Accessible {name:?} did not accept an Action or Selection").into(),
                );
            }
        }
        "set" => {
            let value: f64 = value.ok_or("Missing accessible value")?.parse()?;
            if !value.is_finite() {
                return Err("Nonfinite accessible value".into());
            }
            call(
                &bus,
                &destination,
                &path,
                "org.freedesktop.DBus.Properties",
                "Set",
                Some(&("org.a11y.atspi.Value", "CurrentValue", value.to_variant()).to_variant()),
            )?;
            let actual = property(
                &bus,
                &destination,
                &path,
                "org.a11y.atspi.Value",
                "CurrentValue",
            )?
            .get::<f64>()
            .ok_or("Invalid accessible value")?;
            if (actual - value).abs() > 0.001 {
                return Err(
                    format!("Accessible value remained {actual}, requested {value}").into(),
                );
            }
        }
        _ => return Err("Unknown UI operation".into()),
    }
    Ok(())
}
fn run() -> Result {
    require_sandbox()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |index: usize| -> Result<&str> {
        args.get(index)
            .map(String::as_str)
            .ok_or_else(|| format!("Missing argument {index}").into())
    };
    match arg(0)? {
        "fixtures" => fixtures(Path::new(arg(1)?)),
        "fixture-output" => {
            let properties = openwave_core::routing::pulse_properties(json!({"node.description":"Fixture Output"}).as_object().expect("object"))?;
            println!("{}", command("pactl",&["load-module","module-null-sink","sink_name=fixture_output","channels=2","channel_map=front-left,front-right",&format!("sink_properties={properties}")])?);
            Ok(())
        },
        "tone" => tone(),
        "state" => { println!("{}",state(arg(1)?)?); Ok(()) },
        "action" => activate(APP_PATH,arg(1)?,args.get(2).map(String::as_str)),
        "window-action" => activate(&format!("{APP_PATH}/window/{}",arg(1)?),arg(2)?,args.get(3).map(String::as_str)),
        "activate" => { call(&session()?,APP,APP_PATH,"org.freedesktop.Application","Activate",Some(&(HashMap::<String,glib::Variant>::new(),).to_variant()))?; Ok(()) },
        "identity" => { println!("{}",identity(&graph()?,arg(1)?)?); Ok(()) },
        "wait-node" => wait_for(arg(1)?,|| { node(&graph()?,arg(1)?)?; Ok(()) }),
        "link-fixture" => wait_for("fixture stereo links",||link_fixture(arg(1)?,arg(2)?)),
        "record" => record(arg(1)?,arg(2)?,arg(3)?.parse()?),
        "assert-routes" => wait_for("exclusive intake and missing-output silence",route_assert),
        "break-link" => break_link(Path::new(arg(1)?)),
        "watcher" => watcher(arg(1)?,arg(2)?),
        "tray" => tray(arg(1)?,arg(2)?),
        "ui" => ui(arg(1)?,args.get(2).map_or("",String::as_str),args.get(3).map(String::as_str)),
        _ => Err("Commands: fixtures, fixture-output, tone, state, action, window-action, activate, identity, wait-node, link-fixture, record, assert-routes, break-link, watcher, tray, ui".into()),
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("smoke-control: {error}");
        std::process::exit(1);
    }
}
