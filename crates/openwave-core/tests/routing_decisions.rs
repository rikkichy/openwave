use openwave_core::{
    model::{MixId, NodeIdentity, OutputSnapshot, SourceId, StreamSnapshot, normalize_sources},
    routing::*,
};
use serde_json::{Map, json};

fn identity(serial: &str) -> NodeIdentity {
    NodeIdentity {
        server_cookie: 42,
        object_serial: serial.into(),
    }
}
fn stream(serial: &str, app: &str, node: &str, binary: &str) -> StreamSnapshot {
    StreamSnapshot {
        identity: identity(serial),
        node_id: 1,
        node_name: node.into(),
        app_name: app.into(),
        binary: binary.into(),
        sink: None,
        pulse_index: None,
    }
}
fn output(name: &str, priority: i64, is_wave: bool) -> OutputSnapshot {
    OutputSnapshot {
        identity: identity(name),
        node_id: 1,
        node_name: name.into(),
        name: name.into(),
        priority,
        is_wave,
        properties: Map::new(),
    }
}

#[test]
fn normalized_exact_claims_preserve_duplicate_stream_names_and_stable_ties() {
    let sources = normalize_sources(json!({
        "z": {"match_app_names": [" STRASSE   Radio "]},
        "a": {"match_app_names": ["player"]},
        "b": {"match_app_names": ["straße radio"]},
        "device": {"kind":"device", "match_app_names":["straße radio"]},
        "rest_z": {"catch_all":true}, "rest_a": {"catch_all":true}
    }))
    .unwrap();
    let streams = [
        stream("first", "Straße\tRADIO", "Player", "/usr/bin/player"),
        stream("second", "STRASSE RADIO", "Player", "/usr/bin/player"),
        stream("third", "Straße Radio extra", "Other", "different"),
    ];
    let claims = claim_streams(&sources, &streams);
    assert_eq!(claims["b"], [identity("first"), identity("second")]);
    assert_eq!(claims["rest_a"], [identity("third")]);
    assert!(claims["a"].is_empty() && claims["z"].is_empty() && claims["device"].is_empty());
    let mut reversed = sources.clone();
    reversed.reverse();
    assert_eq!(claim_streams(&reversed, &streams)["b"], claims["b"]);
    let normalized: Vec<_> = streams.iter().map(NormalizedStream::new).collect();
    assert_eq!(
        NormalizedMatcher::new(&sources).claim_normalized(&normalized),
        claims
    );
}

#[test]
fn node_and_full_binary_outrank_basename_without_substring_matching() {
    let sources = normalize_sources(json!({
        "a_base":{"match_app_names":["player"]},
        "z_full":{"match_app_names":["/opt/player"]},
        "z_node":{"match_app_names":["exact node"]}
    }))
    .unwrap();
    let claims = claim_streams(
        &sources,
        &[
            stream("one", "unknown", "Exact Node", "/opt/player"),
            stream("two", "unknown", "Exact Node extended", "/opt/player"),
            stream("three", "unknown", "other", "/usr/bin/player"),
            stream("four", "unknown", "other", "/usr/bin/player-extra"),
        ],
    );
    assert_eq!(claims["z_node"], [identity("one")]);
    assert_eq!(claims["z_full"], [identity("two")]);
    assert_eq!(claims["a_base"], [identity("three")]);
}

#[test]
fn exact_output_missing_or_unmonitored_never_falls_back() {
    let outputs = [
        output("speakers", 200, false),
        output("wave", 1, true),
        output("openwave_chat_mix", 900, false),
    ];
    assert_eq!(
        resolve_output("unplugged", &outputs, Some("speakers")).unwrap(),
        OutputDecision::Silent(SilentOutput::MissingExplicit)
    );
    assert_eq!(
        resolve_output("none", &outputs, Some("speakers")).unwrap(),
        OutputDecision::Silent(SilentOutput::NotMonitored)
    );
    assert_eq!(
        resolve_output("auto", &outputs, Some("speakers")).unwrap(),
        OutputDecision::Monitor(&outputs[1])
    );
    assert!(resolve_output("openwave_chat_mix", &outputs, None).is_err());
    let outputs = [
        output("z", 100, false),
        output("a", 100, false),
        output("default", 1, false),
        output("openwave_hidden", 1000, true),
    ];
    assert_eq!(
        resolve_output("auto", &outputs, Some("default")).unwrap(),
        OutputDecision::Monitor(&outputs[2])
    );
    assert_eq!(
        resolve_output("auto", &outputs, Some("openwave_hidden")).unwrap(),
        OutputDecision::Monitor(&outputs[1])
    );
    assert_eq!(
        resolve_output("auto", &[], None).unwrap(),
        OutputDecision::Silent(SilentOutput::NoEligibleOutput)
    );
    let mut ambiguous = vec![output("same", 1, false), output("same", 1, false)];
    ambiguous[1].identity = identity("other");
    assert_eq!(
        resolve_output("same", &ambiguous, None).unwrap(),
        OutputDecision::Silent(SilentOutput::AmbiguousIdentity)
    );
}

#[test]
fn ready_wave_preference_changes_only_automatic_and_still_requires_unique_identity() {
    let mut outputs = vec![
        output("wave_a", 100, true),
        output("wave_b", 1, true),
        output("speakers", 200, false),
    ];
    assert_eq!(
        resolve_output_with_wave("auto", &outputs, Some("speakers"), Some("wave_b")).unwrap(),
        OutputDecision::Monitor(&outputs[1])
    );
    assert_eq!(
        resolve_output_with_wave("wave_a", &outputs, Some("speakers"), Some("wave_b")).unwrap(),
        OutputDecision::Monitor(&outputs[0])
    );
    assert_eq!(
        resolve_output_with_wave("missing", &outputs, Some("speakers"), Some("wave_b")).unwrap(),
        OutputDecision::Silent(SilentOutput::MissingExplicit)
    );
    outputs.push(output("wave_b", 1, true));
    outputs[3].identity = identity("replacement");
    assert_eq!(
        resolve_output_with_wave("auto", &outputs, Some("speakers"), Some("wave_b")).unwrap(),
        OutputDecision::Silent(SilentOutput::AmbiguousIdentity)
    );
}

#[test]
fn group_handover_silences_first_and_switches_in_row_order() {
    let mut sources = normalize_sources(json!({
        "z":{"group":"Mics"}, "a":{"group":"Mics", "muted":true},
        "b":{"group":"Mics", "muted":true}, "other":{"group":"Other"}
    }))
    .unwrap();
    let change = switch_group(&mut sources, "Mics").unwrap().unwrap();
    assert_eq!(change.source.as_str(), "a");
    assert_eq!(
        change.changes,
        [
            MuteChange {
                source: SourceId::new("z").unwrap(),
                muted: true
            },
            MuteChange {
                source: SourceId::new("a").unwrap(),
                muted: false
            }
        ]
    );
    assert!(!sources["other"].muted);
    set_source_muted(&mut sources, &SourceId::new("a").unwrap(), true).unwrap();
    assert!(
        sources
            .values()
            .filter(|s| s.group == "Mics")
            .all(|s| s.muted)
    );
    assert_eq!(
        switch_group(&mut sources, "Mics")
            .unwrap()
            .unwrap()
            .source
            .as_str(),
        "z"
    );
    assert_eq!(source_groups(&sources), ["Mics"]);
    let before = sources.clone();
    assert!(set_source_muted(&mut sources, &SourceId::new("absent").unwrap(), false).is_err());
    assert_eq!(sources, before);
}

#[test]
fn same_value_open_observation_still_silences_stale_peer() {
    let mut sources = normalize_sources(json!({"one":{"group":"m"}, "two":{"group":"m"}})).unwrap();
    // A caller can reconcile stale observations even when the target value did not change.
    sources.get_mut("two").unwrap().muted = false;
    let changes = set_source_muted(&mut sources, &SourceId::new("one").unwrap(), false).unwrap();
    assert_eq!(
        changes,
        [MuteChange {
            source: SourceId::new("two").unwrap(),
            muted: true
        }]
    );
    assert!(!sources["one"].muted && sources["two"].muted);
}

fn unquote(text: &str) -> String {
    assert!(text.starts_with('"') && text.ends_with('"'));
    let mut chars = text[1..text.len() - 1].chars();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        result.push(if c == '\\' { chars.next().unwrap() } else { c });
    }
    result
}
#[test]
fn hostile_labels_survive_both_pulse_layers_as_one_value() {
    let label = "Music \" } node.name = evil { \\ path\n Straße";
    let values = json!({"node.description": label})
        .as_object()
        .unwrap()
        .clone();
    let outer = unquote(&pulse_properties(&values).unwrap());
    let (key, inner) = outer.split_once('=').unwrap();
    assert_eq!(key, "node.description");
    assert_eq!(unquote(inner), label);
    let spa = properties(&values).unwrap();
    let value = spa
        .strip_prefix("{ node.description = ")
        .unwrap()
        .strip_suffix(" }")
        .unwrap();
    assert_eq!(serde_json::from_str::<String>(value).unwrap(), label);
    assert!(pulse_properties(json!({"x":"nul\u{0000}"}).as_object().unwrap()).is_err());
    assert!(properties(json!({"x = injected":"bad"}).as_object().unwrap()).is_err());
}

#[test]
fn route_names_disambiguate_source_mix_boundary() {
    let first = cell_route_name(&SourceId::new("a_b").unwrap(), &MixId::new("c").unwrap());
    let second = cell_route_name(&SourceId::new("a").unwrap(), &MixId::new("b_c").unwrap());
    assert_ne!(first, second);
}
