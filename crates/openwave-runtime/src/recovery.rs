use crate::process::CommandRunner;
use openwave_core::{
    health::parse_health_graph,
    model::{ErrorCode, NodeIdentity, OperationError, Result},
};
use serde_json::Value;
use std::{
    thread,
    time::{Duration, Instant},
};

pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Narrow execution seam: production always uses the supervised, bounded runner.
pub(crate) trait Commands {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        restoration: bool,
    ) -> Result<Vec<u8>>;
    fn cancelled(&self) -> bool;
    fn wait(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.cancelled() && Instant::now() < deadline {
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50)),
            );
        }
    }
}
impl Commands for CommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        restoration: bool,
    ) -> Result<Vec<u8>> {
        let output = if restoration {
            self.run_uncancelled(program, args, timeout)
        } else {
            CommandRunner::run(self, program, args, timeout)
        }?;
        Ok(output.stdout)
    }
    fn cancelled(&self) -> bool {
        self.is_cancelled()
    }
}
pub(crate) fn check_cancel(runner: &impl Commands) -> Result<()> {
    if runner.cancelled() {
        Err(OperationError::new(
            ErrorCode::Cancelled,
            "Health collection cancelled",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn json_command(
    runner: &impl Commands,
    program: &str,
    args: &[&str],
    restoration: bool,
) -> Result<Value> {
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let output = runner.run(program, &args, COMMAND_TIMEOUT, restoration)?;
    serde_json::from_slice(&output)
        .map_err(|error| OperationError::unavailable(format!("Invalid {program} JSON: {error}")))
}
fn listing(runner: &impl Commands, kind: &str, restoration: bool) -> Result<Vec<Value>> {
    let value = json_command(
        runner,
        "pactl",
        &["--format=json", "list", kind],
        restoration,
    )?;
    let Value::Array(entries) = value else {
        return Err(OperationError::unavailable("Invalid Pulse listing"));
    };
    if entries.iter().any(|entry| !entry.is_object()) {
        return Err(OperationError::unavailable("Invalid Pulse listing entry"));
    }
    Ok(entries)
}
fn identity_error() -> OperationError {
    OperationError::new(
        ErrorCode::Identity,
        "Recovery target is absent, ambiguous or has changed identity",
    )
}
fn scalar(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}
fn unique<'a>(entries: &'a [Value], key: &str, value: &str) -> Result<&'a Value> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.get(key).and_then(scalar).as_deref() == Some(value));
    let found = matches.next().ok_or_else(identity_error)?;
    if matches.next().is_some() {
        return Err(identity_error());
    }
    Ok(found)
}
fn read_graph(runner: &impl Commands, restoration: bool) -> Result<Value> {
    let graph = json_command(runner, "pw-dump", &[], restoration)?;
    // Validate each discovery once; identities are not an empty-graph fallback.
    parse_health_graph(&graph)?;
    Ok(graph)
}
/// Includes Core identity even if a profile-off operation temporarily removes all nodes.
fn graph_identity(graph: &Value, kind: &str, name_key: &str, name: &str) -> Result<NodeIdentity> {
    let entries = graph.as_array().ok_or_else(identity_error)?;
    let core = entries
        .iter()
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Core"))
        .ok_or_else(identity_error)?;
    let cookie = core
        .pointer("/info/cookie")
        .and_then(scalar)
        .and_then(|s| s.parse().ok())
        .ok_or_else(identity_error)?;
    let mut matches = entries.iter().filter(|entry| {
        entry.get("type").and_then(Value::as_str) == Some(kind)
            && entry
                .pointer("/info/props")
                .and_then(|p| p.get(name_key))
                .and_then(Value::as_str)
                == Some(name)
    });
    let entry = matches.next().ok_or_else(identity_error)?;
    if matches.next().is_some() {
        return Err(identity_error());
    }
    let props = entry
        .pointer("/info/props")
        .and_then(Value::as_object)
        .ok_or_else(identity_error)?;
    let serial = match props.get("object.serial") {
        Some(value) => scalar(value),
        None => entry.get("id").and_then(scalar),
    }
    .ok_or_else(identity_error)?;
    Ok(NodeIdentity {
        server_cookie: cookie,
        object_serial: serial,
    })
}
pub(crate) fn validate_node(
    runner: &impl Commands,
    name: &str,
    identity: &NodeIdentity,
    restoration: bool,
) -> Result<Value> {
    let graph = read_graph(runner, restoration)?;
    if graph_identity(&graph, "PipeWire:Interface:Node", "node.name", name)? != *identity {
        return Err(identity_error());
    }
    Ok(graph)
}
fn validate_sink(
    runner: &impl Commands,
    name: &str,
    identity: &NodeIdentity,
    restoration: bool,
) -> Result<()> {
    validate_node(runner, name, identity, restoration)?;
    let sinks = listing(runner, "sinks", restoration)?;
    let sink = unique(&sinks, "name", name)?;
    if sink
        .pointer("/properties/object.serial")
        .and_then(scalar)
        .as_deref()
        != Some(identity.object_serial.as_str())
    {
        return Err(identity_error());
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct CardTarget {
    name: String,
    index: String,
    identity: NodeIdentity,
    profile: String,
}
fn resolve_card(runner: &impl Commands, name: &str, identity: &NodeIdentity) -> Result<CardTarget> {
    let kind = if name.starts_with("alsa_input.") {
        "sources"
    } else if name.starts_with("alsa_output.") {
        "sinks"
    } else {
        return Err(identity_error());
    };
    let graph = validate_node(runner, name, identity, false)?;
    let nodes = listing(runner, kind, false)?;
    let node = unique(&nodes, "name", name)?;
    // Pulse and native graph must refer to the same node generation.
    if node
        .pointer("/properties/object.serial")
        .and_then(scalar)
        .as_deref()
        != Some(identity.object_serial.as_str())
    {
        return Err(identity_error());
    }
    let index = node
        .get("card")
        .and_then(scalar)
        .ok_or_else(identity_error)?;
    let cards = listing(runner, "cards", false)?;
    let card = unique(&cards, "index", &index)?;
    let card_name = card
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(identity_error)?;
    unique(&cards, "name", card_name)?;
    let profile = card
        .get("active_profile")
        .and_then(|v| v.as_str().or_else(|| v.get("name").and_then(Value::as_str)))
        .filter(|s| !s.is_empty() && *s != "off")
        .ok_or_else(|| OperationError::unavailable("Card has no active profile to restore"))?;
    let card_identity = graph_identity(
        &graph,
        "PipeWire:Interface:Device",
        "device.name",
        card_name,
    )?;
    Ok(CardTarget {
        name: card_name.into(),
        index,
        identity: card_identity,
        profile: profile.into(),
    })
}
fn restore_card(runner: &impl Commands, target: &CardTarget) -> Result<()> {
    let graph = read_graph(runner, true)?;
    if graph_identity(
        &graph,
        "PipeWire:Interface:Device",
        "device.name",
        &target.name,
    )? != target.identity
    {
        return Err(identity_error());
    }
    let cards = listing(runner, "cards", true)?;
    let card = unique(&cards, "name", &target.name)?;
    if card.get("index").and_then(scalar).as_deref() != Some(target.index.as_str()) {
        return Err(identity_error());
    }
    command(
        runner,
        &["set-card-profile", &target.name, &target.profile],
        true,
    )
}
fn command(runner: &impl Commands, args: &[&str], restoration: bool) -> Result<()> {
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    runner
        .run("pactl", &args, COMMAND_TIMEOUT, restoration)
        .map(|_| ())
}
fn finish(changed: Result<()>, restored: Result<()>) -> Result<()> {
    match (changed, restored) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(OperationError::unavailable(format!(
            "Audio restoration failed: {error}"
        ))),
        (Err(error), Err(restore)) => Err(OperationError::unavailable(format!(
            "{error}; audio restoration failed: {restore}"
        ))),
    }
}

/// Owns the exact restoration owed by a potentially applied remedy. Keep this
/// owner alive across observations and shutdown retries until `restore` succeeds.
#[derive(Default)]
pub struct Restoration {
    pending: Option<Obligation>,
}
enum Obligation {
    Card(CardTarget),
    Sink {
        name: String,
        identity: NodeIdentity,
    },
}
impl Restoration {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn restore(&mut self, runner: &CommandRunner) -> Result<()> {
        self.restore_with(runner)
    }
    pub(crate) fn restore_with(&mut self, runner: &impl Commands) -> Result<()> {
        let result = match &self.pending {
            None => return Ok(()),
            Some(Obligation::Card(target)) => restore_card(runner, target),
            Some(Obligation::Sink { name, identity }) => {
                validate_sink(runner, name, identity, true)
                    .and_then(|_| command(runner, &["suspend-sink", name, "0"], true))
            }
        };
        result.map_err(|error| {
            OperationError::unavailable(format!("Audio restoration remains owed: {error}"))
        })?;
        self.pending = None;
        Ok(())
    }
    fn require_clear(&self) -> Result<()> {
        if self.is_pending() {
            Err(OperationError::unavailable(
                "Cannot begin recovery while audio restoration remains owed",
            ))
        } else {
            Ok(())
        }
    }
}
/// Caller has explicitly opted in and spent the stable-name incident budget.
pub fn cycle_card(
    node_name: &str,
    identity: &NodeIdentity,
    runner: &CommandRunner,
    restoration: &mut Restoration,
) -> Result<()> {
    cycle_card_with(node_name, identity, runner, restoration)
}
pub(crate) fn cycle_card_with(
    node_name: &str,
    identity: &NodeIdentity,
    runner: &impl Commands,
    restoration: &mut Restoration,
) -> Result<()> {
    restoration.require_clear()?;
    check_cancel(runner)?;
    let target = resolve_card(runner, node_name, identity)?;
    // Mapping/profile may change during collection. Never switch a replaced target.
    if resolve_card(runner, node_name, identity)? != target {
        return Err(identity_error());
    }
    check_cancel(runner)?;
    let changed = match restoration.pending.insert(Obligation::Card(target)) {
        Obligation::Card(target) => {
            command(runner, &["set-card-profile", &target.name, "off"], false)
        }
        Obligation::Sink { .. } => unreachable!(),
    };
    // Even a failed/cancelled/timed-out command may already have applied off.
    let restored = restoration.restore_with(runner);
    finish(changed, restored)
}
/// Suspend only this exact observed output, then owe resume even on timeout.
pub fn recycle_sink(
    node_name: &str,
    identity: &NodeIdentity,
    runner: &CommandRunner,
    restoration: &mut Restoration,
) -> Result<()> {
    recycle_sink_with(node_name, identity, runner, restoration)
}
pub(crate) fn recycle_sink_with(
    node_name: &str,
    identity: &NodeIdentity,
    runner: &impl Commands,
    restoration: &mut Restoration,
) -> Result<()> {
    restoration.require_clear()?;
    if !node_name.starts_with("alsa_output.") {
        return Err(identity_error());
    }
    check_cancel(runner)?;
    validate_sink(runner, node_name, identity, false)?;
    check_cancel(runner)?;
    restoration.pending = Some(Obligation::Sink {
        name: node_name.into(),
        identity: identity.clone(),
    });
    let changed = command(runner, &["suspend-sink", node_name, "1"], false);
    if changed.is_ok() {
        runner.wait(Duration::from_secs(1));
    }
    let restored = restoration.restore_with(runner);
    finish(changed, restored)
}
