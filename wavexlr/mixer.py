"""Worker-owned PipeWire routing. Public reads are detached cached snapshots.

A configured application owns an intake even with all sends at zero: its audio
cannot bypass a zero fader. One deterministic claim and one move replace copying
an application's already-playing output. Graph observations, not child liveness,
determine whether links and levels need repair.
"""

import atexit
import copy
import json
import logging
import os
import subprocess
import threading
import uuid

from . import child, mixes as mix_store, sources

_log = logging.getLogger(__name__)
CONFIG_PATH = os.path.join(os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "openwave", "mixes.json")
MIX_SINKS = {"personal": "openwave_personal_mix", "chat": "openwave_chat_mix", "record": "openwave_record_mix"}
CARD_NAME_TOKENS = ("Elgato_Wave_", "Elgato_XLR_Dock")
OUTPUT_AUTO = "auto"
OUTPUT_NONE = "none"
OUTPUTS_STATE_KEY = "outputs"
VOLUMES_STATE_KEY = "volumes"


def properties(values):
    """SPA property object; JSON quoting also escapes untrusted display labels."""
    return "{ " + " ".join(f"{key} = {json.dumps(value, ensure_ascii=True, allow_nan=False)}" for key, value in values.items()) + " }"

def pulse_properties(values):
    """Quote both Pulse argument layers; its parser is not a JSON decoder."""
    def quote(text):
        if "\0" in text:
            raise ValueError("Audio properties cannot contain NUL")
        return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'

    pairs = []
    for key, value in values.items():
        text = value if isinstance(value, str) else json.dumps(value, allow_nan=False)
        pairs.append(f"{key}={quote(text)}")
    return quote(" ".join(pairs))



def _is_wave_card(node_name):
    return any(token in node_name for token in CARD_NAME_TOKENS)

def source_sink_name(source_id):
    return "openwave_src_" + sources.safe_id(source_id)


def mix_capture_name(mix_id):
    return "openwave_capture_" + sources.safe_id(mix_id)


def _normalize(value):
    return " ".join(str(value or "").split()).casefold()


def _match_rank(source, stream):
    if sources.kind(source) != sources.KIND_APP:
        return None
    wanted = {_normalize(name) for name in sources.bindings(source)} - {""}
    binary = str(stream.get("binary") or "")
    identities = (stream.get("app_name"), stream.get("node_name"), binary, os.path.basename(binary))
    return next((rank for rank, identity in enumerate(identities) if _normalize(identity) in wanted), None)


def stream_matches(source, stream):
    return _match_rank(source, stream) is not None


def claim_streams(records, streams):
    """Exact normalized identity matches; specificity then stable id break ties."""
    claims = {sid: set() for sid in records}
    fallback = sorted(sid for sid, source in records.items() if sources.kind(source) == sources.KIND_APP and source.get("catch_all"))
    for stream_id, stream in streams.items():
        candidates = []
        for sid, source in records.items():
            rank = _match_rank(source, stream)
            if rank is not None:
                candidates.append((rank, sid))
        owner = min(candidates)[1] if candidates else (fallback[0] if fallback else None)
        if owner is not None:
            claims[owner].add(stream_id)
    return claims


class GraphError(RuntimeError):
    """A command failed or a graph snapshot is incomplete; retry next cycle."""


class SubprocessPipeWire:
    """Single worker's process boundary. Mutation methods return success only.

    Discovery raises GraphError rather than returning an empty graph on failure.
    Module handles include an unguessable owner token so a restarted server's
    reused module index can never authorize deletion of someone else's resource.
    """

    @staticmethod
    def run(argv):
        try:
            result = subprocess.run(argv, capture_output=True, text=True, timeout=3)
        except (OSError, subprocess.SubprocessError) as exc:
            raise GraphError(str(exc)) from exc
        if result.returncode:
            raise GraphError(f"{argv[0]} exited {result.returncode}: {result.stderr.strip()}")
        return result.stdout

    def command(self, argv):
        try:
            self.run(argv)
            return True
        except GraphError as exc:
            _log.warning("%s", exc)
            return False

    def snapshot(self):
        try:
            objects = json.loads(self.run(["pw-dump"]))
            pulse = {kind: json.loads(self.run(["pactl", "--format=json", "list", kind]))
                     for kind in ("sinks", "sources", "sink-inputs", "modules")}
            devices = {str(obj["id"]): (obj.get("info") or {}).get("props", {})
                       for obj in objects if obj.get("type") == "PipeWire:Interface:Device"}
            if not isinstance(objects, list) or any(not isinstance(items, list) for items in pulse.values()):
                raise ValueError("Expected graph arrays")
            generation = next(((obj.get("info") or {}).get("cookie") for obj in objects
                               if obj.get("type") == "PipeWire:Interface:Core"), None)
            default = self.run(["pactl", "get-default-sink"]).strip() if pulse["sinks"] else None
            nodes = {}
            all_nodes = []
            ports = {}
            links = set()
            for obj in objects:
                info = obj.get("info") or {}
                props = info.get("props") or {}
                typ = obj.get("type", "").rsplit(":", 1)[-1]
                if typ == "Node":
                    name = props.get("node.name", "")
                    nodes[name] = {"id": obj["id"], "serial": str(props.get("object.serial", obj["id"])), "props": props}
                    all_nodes.append((name, nodes[name]))
                elif typ == "Port":
                    key = (str(props.get("node.id")), props.get("port.direction"))
                    ports.setdefault(key, []).append({"id": obj["id"], "channel": props.get("audio.channel", "")})
                elif typ == "Link":
                    links.add((info["output-port-id"], info["input-port-id"]))
            sinks = {}
            for sink in pulse["sinks"]:
                node = nodes.get(sink["name"])
                if node is None:
                    continue
                volumes = [v["value"] / 65536 for v in sink.get("volume", {}).values()]
                sinks[sink["name"]] = {"name": sink["name"], "index": sink["index"], "description": sink.get("description", sink["name"]),
                    "identity": (generation, node["serial"]), "volume": max(volumes, default=1.0), "muted": bool(sink.get("mute")),
                    "priority": int(node["props"].get("priority.session", 0))}
            sink_indices = {str(sink["index"]): name for name, sink in sinks.items()}
            inputs = {str(item.get("properties", {}).get("object.serial", "")): item for item in pulse["sink-inputs"]}
            streams = {}
            captures = []
            for name, node in all_nodes:
                props = node["props"]
                media_class = props.get("media.class", "")
                if media_class == "Stream/Output/Audio" and not name.startswith("openwave_"):
                    pulse_input = inputs.get(node["serial"], {})
                    streams[node["id"]] = {"id": node["id"], "serial": node["serial"],
                        "pulse_id": pulse_input.get("index"), "sink": sink_indices.get(str(pulse_input.get("sink"))),
                        "app_name": props.get("application.name") or name, "node_name": name,
                        "media_name": props.get("media.name", ""), "binary": props.get("application.process.binary", "")}
                if media_class == "Audio/Source" and not name.startswith("openwave_") and not name.endswith(".monitor"):
                    device = devices.get(str(props.get("device.id")), {})
                    captures.append({
                        "name": name, "description": props.get("node.description", name),
                        "priority": int(props.get("priority.session", 0)),
                        "identity": (generation, node["serial"]),
                        "channels": int(props.get("audio.channels", 2)),
                        "alsa_card": props.get("api.alsa.pcm.card", device.get("api.alsa.card")),
                        "serial": props.get("device.serial", device.get("device.serial")),
                    })
            mutes = {item["name"]: bool(item.get("mute")) for item in pulse["sources"]}
            return {"nodes": nodes, "ports": ports, "links": links, "sinks": sinks, "streams": streams,
                    "captures": captures, "capture_mutes": mutes, "modules": pulse["modules"], "default": default,
                    "generation": generation, "sink_names": {sink["name"] for sink in pulse["sinks"]}}
        except (ValueError, TypeError, KeyError) as exc:
            raise GraphError(f"Malformed PipeWire snapshot: {exc}") from exc

    def move_stream(self, stream, sink):
        # Native streams also appear in pipewire-pulse's sink-input listing.
        # Never guess an index or copy a stream whose move was rejected.
        index = stream.get("pulse_id")
        return index is not None and self.command(["pactl", "move-sink-input", str(index), sink])

    def link(self, source_port, target_port):
        return self.command(["pw-link", str(source_port), str(target_port)])

    def set_level(self, node_id, volume, muted):
        volume_ok = self.command(["wpctl", "set-volume", str(node_id), str(volume)])
        mute_ok = self.command(["wpctl", "set-mute", str(node_id), "1" if muted else "0"])
        return volume_ok and mute_ok

    def set_capture_mute(self, name, muted):
        return self.command(["pactl", "set-source-mute", name, "1" if muted else "0"])

    def create_sink(self, name, description):
        token = uuid.uuid4().hex
        props = pulse_properties({"node.description": description, "media.name": name, "openwave.owner": token,
                                  "priority.session": 0, "monitor.channel-volumes": True, "state.restore-props": False})
        try:
            module_id = int(self.run(["pactl", "load-module", "module-null-sink", "sink_name=" + name,
                                      "channels=2", "channel_map=front-left,front-right", "sink_properties=" + props]).strip())
        except (GraphError, ValueError):
            return None
        return (module_id, token)

    def destroy_sink(self, handle, graph):
        module_id, token = handle
        owned = any(str(node["props"].get("pulse.module.id")) == str(module_id)
                    and node["props"].get("openwave.owner") == token
                    for node in graph["nodes"].values())
        return not owned or self.command(["pactl", "unload-module", str(module_id)])

    def remove_mix_sink(self, name, graph):
        """Explicit definition removal, never used by application shutdown."""
        node = graph["nodes"].get(name)
        return node is None or self.command(["pw-cli", "destroy", str(node["id"])])

    @staticmethod
    def spawn_loopback(name, publish=False, description=None):
        capture_name = name + "_cap"
        capture = {"node.name": capture_name, "media.name": capture_name, "node.autoconnect": False,
                   "audio.channels": 2, "audio.position": ["FL", "FR"]}
        playback = {"node.name": name, "media.name": name, "node.autoconnect": False,
                    "audio.channels": 2, "audio.position": ["FL", "FR"]}
        if publish:
            playback.update({"media.class": "Audio/Source", "node.virtual": True,
                             "node.description": description or name})
        try:
            return child.spawn(["pw-loopback", "--capture-props=" + properties(capture),
                                "--playback-props=" + properties(playback)], stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
        except OSError:
            return None


class Mixer:
    """Desired state setters enqueue work; stop is a synchronous drain barrier.

    A failed discovery leaves the last snapshot intact. Failed moves, links and
    levels are retried every poll; no success is inferred from a living child.
    """

    def __init__(self, pw=None, *, config_path=None, mixes_config_path=None, poll_interval=1.0, capture_ready=None):
        self._pw = pw or SubprocessPipeWire()
        self._capture_ready = capture_ready
        self._path = config_path or CONFIG_PATH
        self._mixes_config_path = mixes_config_path
        self._lock = threading.RLock()
        self._save_lock = threading.Lock()
        self._wake = threading.Event()
        self._stopping = threading.Event()
        self._thread = None
        self._closed = False
        self._poll_interval = poll_interval
        try:
            with open(self._path) as f:
                self._state = json.load(f)
            if not isinstance(self._state, dict):
                raise ValueError("Matrix state must be an object")
            for key, cell in self._state.items():
                if "." in key:
                    sid, mid = key.split(".")
                    sources.safe_id(sid)
                    sources.safe_id(mid)
                    cell["volume"] = sources.level(cell.get("volume", 0))
                    cell["muted"] = bool(cell.get("muted", False))
        except FileNotFoundError:
            self._state = {}
        for key in (OUTPUTS_STATE_KEY, VOLUMES_STATE_KEY):
            if not isinstance(self._state.setdefault(key, {}), dict):
                raise ValueError(f"Matrix {key} must be an object")
        legacy_output = self._state.pop("output", None)
        if isinstance(legacy_output, str):
            self._state[OUTPUTS_STATE_KEY].setdefault("personal", legacy_output)
        for mid, entry in self._state[VOLUMES_STATE_KEY].items():
            sources.safe_id(mid)
            entry["volume"] = sources.level(entry["volume"])
            entry["muted"] = bool(entry.get("muted", False))
        for mid, output in self._state[OUTPUTS_STATE_KEY].items():
            sources.safe_id(mid)
            if not isinstance(output, str) or not output:
                raise ValueError("Output must be a sink name, auto or none")
        self._sources = {}
        self._mixes = copy.deepcopy(mix_store.DEFAULT_MIXES)
        self._definitions_revision = 0
        self._definitions_written = 0
        self._removed_sinks = set()
        self._master_revisions = {}
        self._master_restored = {}
        self._master_pending = {}
        self._restored_snapshot = frozenset()
        self._capture_requests = {}
        self._graph = None
        self._error = None
        self._procs = {}
        self._owned_sinks = {}
        self._moved = {}
        self.mic = self.hp = None
        atexit.register(self.stop)

    def _persist(self):
        with self._save_lock:
            with self._lock:
                state = copy.deepcopy(self._state)
            sources._atomic_write(self._path, state)

    def get_cell(self, source_id, mix_id):
        with self._lock:
            return dict(self._state.get(f"{source_id}.{mix_id}", {"volume": 0.0, "muted": False}))

    def cells(self):
        with self._lock:
            return copy.deepcopy({key: value for key, value in self._state.items() if "." in key})

    def streams(self):
        with self._lock:
            return copy.deepcopy(self._graph["streams"] if self._graph else {})

    def last_error(self):
        with self._lock:
            return self._error

    def output_sinks(self):
        with self._lock:
            sinks = self._graph["sinks"].values() if self._graph else ()
            return copy.deepcopy(sorted((sink for sink in sinks if not sink["name"].startswith("openwave_")),
                                        key=lambda sink: (-sink["priority"], sink["name"])))

    def default_sink(self):
        with self._lock:
            return self._graph["default"] if self._graph else None

    def capture_sources(self):
        with self._lock:
            return copy.deepcopy(self._graph["captures"] if self._graph else [])

    def live_captures(self):
        return frozenset(item["name"] for item in self.capture_sources())

    def capture_mutes(self):
        with self._lock:
            if not self._graph:
                return {}
            live = {item["name"] for item in self._graph["captures"]}
            return {name: muted for name, muted in self._graph["capture_mutes"].items() if name in live}

    def set_capture_mute(self, node_name, muted):
        with self._lock:
            self._capture_requests[node_name] = bool(muted)
        self._wake.set()

    def capture_device_present(self, node_name):
        return node_name in self.live_captures()

    def set_mixes(self, records):
        normalized = mix_store.normalize(records)
        with self._lock:
            self._removed_sinks.update(mix["sink"] for mid, mix in self._mixes.items()
                                       if mid not in normalized or mix["sink"] != normalized[mid]["sink"])
            self._mixes = normalized
            self._definitions_revision += 1
            self._state = {key: value for key, value in self._state.items()
                           if "." not in key or key.rsplit(".", 1)[1] in normalized}
            for key in (OUTPUTS_STATE_KEY, VOLUMES_STATE_KEY):
                self._state[key] = {mid: value for mid, value in self._state[key].items() if mid in normalized}
        self._persist()
        self._wake.set()

    def remove_mix(self, mix_id):
        with self._lock:
            records = copy.deepcopy(self._mixes)
        records.pop(mix_id, None)
        self.set_mixes(records)

    def get_output(self, mix_id):
        with self._lock:
            return self._state[OUTPUTS_STATE_KEY].get(mix_id, OUTPUT_AUTO if mix_id == "personal" else OUTPUT_NONE)

    def set_output(self, mix_id, output):
        if not isinstance(output, str) or not output or output.startswith("openwave_"):
            raise ValueError("Output must be auto, none or a non-OpenWave sink name")
        with self._lock:
            if mix_id not in self._mixes:
                raise KeyError(mix_id)
            self._state[OUTPUTS_STATE_KEY][mix_id] = output
        self._persist()
        self._wake.set()

    def resolve_output(self, mix_id, sinks=None, default_sink=None):
        """Cached resolution. Explicit unplugged outputs stay silent, not rerouted."""
        choice = self.get_output(mix_id)
        if choice == OUTPUT_NONE:
            return None
        candidates = self.output_sinks() if sinks is None else sinks
        eligible = {item["name"] for item in candidates if not item["name"].startswith("openwave_")}
        if choice != OUTPUT_AUTO:
            return choice if choice in eligible else None
        default = self.default_sink() if default_sink is None else default_sink
        for name in (self.hp, default):
            if name in eligible:
                return name
        ordered = sorted(candidates, key=lambda item: (-item.get("priority", 0), item["name"]))
        return next((item["name"] for item in ordered if item["name"] in eligible), None)

    def mix_volume(self, mix_id):
        with self._lock:
            entry = self._state[VOLUMES_STATE_KEY].get(mix_id)
            return (entry["volume"], entry["muted"]) if entry else None

    def set_mix_volume(self, mix_id, volume, muted=None):
        value = sources.level(volume)
        with self._lock:
            if mix_id not in self._mixes:
                raise KeyError(mix_id)
            previous = self._state[VOLUMES_STATE_KEY].get(mix_id, {})
            self._state[VOLUMES_STATE_KEY][mix_id] = {"volume": value, "muted": previous.get("muted", False) if muted is None else bool(muted)}
            self._master_revisions[mix_id] = self._master_revisions.get(mix_id, 0) + 1
        self._persist()
        self._wake.set()

    @property
    def volumes_restored(self):
        with self._lock:
            return set(self._mixes).issubset(self._restored_snapshot)

    def set_cell(self, source_id, mix_id, volume, muted):
        sources.safe_id(source_id)
        sources.safe_id(mix_id)
        volume = sources.level(volume)
        with self._lock:
            self._state[f"{source_id}.{mix_id}"] = {"volume": volume if volume >= 0.01 else 0.0, "muted": bool(muted)}
        self._persist()
        self._wake.set()

    def set_sources(self, records):
        normalized = sources.normalize(records)
        with self._lock:
            self._sources = normalized
        self._wake.set()

    def set_source_level(self, source_id, volume, muted):
        value = sources.level(volume)
        with self._lock:
            self._sources[source_id].update(level=value, muted=bool(muted))
        # Source records are persisted by their store owner, not by this worker.
        self._wake.set()

    def remove_source(self, source_id):
        with self._lock:
            if sources.is_protected(self._sources.get(source_id, {})):
                raise ValueError("Protected source cannot be removed")
            self._sources.pop(source_id, None)
            self._state = {key: value for key, value in self._state.items() if not key.startswith(source_id + ".")}
        self._persist()
        self._wake.set()

    def request_stream_poll(self):
        self._wake.set()

    def request_capture_poll(self):
        self._wake.set()

    def request_volume_sync(self):
        self._wake.set()


    def start(self):
        with self._lock:
            if self._closed:
                raise RuntimeError("A stopped Mixer cannot restart")
            if self._thread is not None:
                return
            self._thread = threading.Thread(target=self._worker_loop, name="openwave-mixer", daemon=True)
            self._thread.start()

    def stop(self):
        with self._lock:
            self._closed = True
            thread = self._thread
        self._stopping.set()
        self._wake.set()
        if thread is not None and thread is not threading.current_thread():
            # Every command is bounded. Never tear down while the worker can
            # still spawn or mutate resources, even if a command is slow.
            thread.join()
        atexit.unregister(self.stop)

    def _worker_loop(self):
        try:
            while not self._stopping.is_set():
                self._wake.clear()
                try:
                    graph = self._pw.snapshot()
                    self._discover(graph)
                    self._reconcile(graph)
                    with self._lock:
                        self._error = None
                except Exception as exc:
                    with self._lock:
                        self._error = str(exc)
                    _log.warning("Routing reconciliation failed: %s", exc)
                self._wake.wait(self._poll_interval)
        finally:
            self._teardown()

    def _discover(self, graph):
        if self._graph is not None and self._graph.get("generation") != graph.get("generation"):
            self._moved.clear()
            self._owned_sinks.clear()
            self._master_restored.clear()
            self._master_pending.clear()
        captures = sorted(item["name"] for item in graph["captures"] if any(token in item["name"] for token in CARD_NAME_TOKENS))
        mic = captures[0] if captures else None
        stem = mic.removeprefix("alsa_input.").rsplit(".", 1)[0] if mic else None
        hp = next((name for name in sorted(graph["sinks"]) if stem and name.startswith("alsa_output." + stem + ".")), None)
        if mic is None:
            hp = next((name for name in sorted(graph["sinks"]) if any(token in name for token in CARD_NAME_TOKENS)), None)
        with self._lock:
            self._graph = graph
            self.mic, self.hp = mic, hp

    def _ensure_sink(self, name, description, graph):
        if name in graph["sinks"]:
            return True
        if name in graph.get("sink_names", set()):
            return False
        handle = self._owned_sinks.get(name)
        if handle is not None:
            if any(handle[1] in str(module.get("argument", "")) for module in graph["modules"]):
                return False
            del self._owned_sinks[name]
        handle = self._pw.create_sink(name, description)
        if handle is not None:
            self._owned_sinks[name] = handle
        return False

    @staticmethod
    def _ports(graph, name, direction):
        node = graph["nodes"].get(name)
        return sorted(graph["ports"].get((str(node["id"]), direction), []), key=lambda port: port["id"]) if node else []

    def _link_nodes(self, graph, source, target):
        outputs = self._ports(graph, source, "out")
        inputs = self._ports(graph, target, "in")
        if not outputs or not inputs:
            return False
        complete = True
        for index, dest in enumerate(inputs):
            origin = next((port for port in outputs if port["channel"] and port["channel"] == dest["channel"]), outputs[index % len(outputs)])
            edge = (origin["id"], dest["id"])
            if edge not in graph["links"]:
                if self._pw.link(*edge):
                    graph["links"].add(edge)
                else:
                    complete = False
        return complete

    @staticmethod
    def _reap(proc):
        if proc.poll() is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    def _drop_route(self, key):
        route = self._procs.pop(key, None)
        if route is not None:
            self._reap(route["proc"])

    def _route(self, key, source, target, name, volume, muted, graph, *, publish=False, description=None):
        spec = (source, target, publish, description)
        route = self._procs.get(key)
        if route is not None and (route["spec"] != spec or route["proc"].poll() is not None):
            self._drop_route(key)
            route = None
        if route is None:
            proc = self._pw.spawn_loopback(name, publish=publish, description=description)
            if proc is None:
                return False
            self._procs[key] = {"proc": proc, "spec": spec, "name": name, "missing": 0}
            return False
        node = graph["nodes"].get(name)
        if node is None:
            route["missing"] += 1
            if route["missing"] >= 3:
                self._drop_route(key)
            return False
        route["missing"] = 0
        applied = (graph.get("generation"), node.get("serial", node["id"]), volume, muted)
        if route.get("applied") != applied:
            if not self._pw.set_level(node["id"], volume, muted):
                return False
            route["applied"] = applied
        # Establish gain before audio can enter a newly created stream.
        playback_ok = publish or self._link_nodes(graph, name, target)
        capture_ok = self._link_nodes(graph, source, name + "_cap") if playback_ok else False
        return playback_ok and capture_ok

    def _sync_masters(self, graph, mixes):
        """Restore each runtime identity, confirm it, then allow observation.

        A reused node.name is not evidence that a previous restore still holds.
        In particular, a replacement unity sink must never overwrite disk before
        the remembered value has been successfully set and observed.
        """
        ready = set()
        changed = False
        for mid, mix in mixes.items():
            sink = graph["sinks"].get(mix["sink"])
            if sink is None:
                self._master_restored.pop(mid, None)
                self._master_pending.pop(mid, None)
                continue
            with self._lock:
                revision = self._master_revisions.get(mid, 0)
                stored = copy.deepcopy(self._state[VOLUMES_STATE_KEY].get(mid))
            identity = (sink["identity"], revision)
            observed = {"volume": sources.level(sink["volume"]), "muted": sink["muted"]}
            if self._master_restored.get(mid) == identity:
                ready.add(mid)
                with self._lock:
                    if self._master_revisions.get(mid, 0) == revision and stored != observed:
                        self._state[VOLUMES_STATE_KEY][mid] = observed
                        changed = True
                continue
            desired = stored or observed
            pending = self._master_pending.get(mid)
            confirmed = pending == (identity, desired) and abs(observed["volume"] - desired["volume"]) <= 0.001 and observed["muted"] == desired["muted"]
            if confirmed:
                self._master_restored[mid] = identity
                self._master_pending.pop(mid, None)
                ready.add(mid)
                if stored is None:
                    with self._lock:
                        if self._master_revisions.get(mid, 0) == revision:
                            self._state[VOLUMES_STATE_KEY][mid] = desired
                            changed = True
            else:
                node = graph["nodes"].get(mix["sink"])
                if node and self._pw.set_level(node["id"], desired["volume"], desired["muted"]):
                    self._master_pending[mid] = (identity, desired)
        with self._lock:
            self._restored_snapshot = frozenset(ready)
        if changed:
            self._persist()
        return ready

    def _sync_capture_mutes(self, graph):
        with self._lock:
            pending = dict(self._capture_requests)
        live = {item["name"] for item in graph["captures"]}
        for name, muted in pending.items():
            if name in live and self._pw.set_capture_mute(name, muted):
                with self._lock:
                    if self._capture_requests.get(name) == muted:
                        del self._capture_requests[name]

    def _reconcile(self, graph):
        with self._lock:
            records, mixes, state = copy.deepcopy((self._sources, self._mixes, self._state))
            definitions_revision = self._definitions_revision
        if definitions_revision != self._definitions_written:
            from . import setup
            setup.install_mixes(mixes, path=self._mixes_config_path)
            self._definitions_written = definitions_revision
        self._sync_capture_mutes(graph)
        wanted_sinks = {mix["sink"] for mix in mixes.values()}
        ready = {mid for mid, mix in mixes.items() if self._ensure_sink(mix["sink"], mix["description"], graph)}
        ready &= self._sync_masters(graph, mixes)
        claims = claim_streams(records, graph["streams"])
        desired = set()
        # Silence departing/muted sends before any group hand-over opens another.
        for key, route in list(self._procs.items()):
            if key[0] != "cell":
                continue
            _, sid, mid = key
            source = records.get(sid)
            cell = state.get(f"{sid}.{mid}", {})
            if source is None or mid not in mixes or cell.get("volume", 0.0) <= 0:
                self._drop_route(key)
                continue
            if source.get("muted") or cell.get("muted"):
                self._route(key, route["spec"][0], route["spec"][1], route["name"],
                            cell["volume"] * source.get("level", 1.0), True, graph)
                if route["name"] in graph["nodes"] and not route.get("applied", (False,))[-1]:
                    raise GraphError("Cannot silence the previous source; group hand-over deferred")
        wanted_moves = {}
        for sid, source in records.items():
            if sources.kind(source) == sources.KIND_APP:
                capture = source_sink_name(sid)
                for stream_id in claims[sid]:
                    wanted_moves[graph["streams"][stream_id]["serial"]] = capture
                wanted_sinks.add(capture)
                if not self._ensure_sink(capture, source["name"], graph):
                    continue
                for stream_id in claims[sid]:
                    stream = graph["streams"][stream_id]
                    if stream["sink"] != capture:
                        if self._pw.move_stream(stream, capture):
                            self._moved.setdefault(stream["serial"], stream["sink"])
            else:
                capture = source.get("node_name")
                if capture not in {item["name"] for item in graph["captures"]}:
                    continue
            for mid in ready:
                cell = state.get(f"{sid}.{mid}", {})
                volume = cell.get("volume", 0.0)
                if volume <= 0:
                    continue
                key = ("cell", sid, mid)
                desired.add(key)
                name = f"openwave_loop_{len(sid)}_{sid}_{mid}"
                self._route(key, capture, mixes[mid]["sink"], name, volume * source.get("level", 1.0),
                            bool(cell.get("muted") or source.get("muted")), graph)
        for mid in ready:
            mix = mixes[mid]
            key = ("capture", mid)
            desired.add(key)
            self._route(key, mix["sink"], None, mix_capture_name(mid), 1.0, False, graph,
                        publish=True, description=mix["description"])
            output = self.resolve_output(mid)
            if output and self._capture_ready is not None and _is_wave_card(output):
                stem = output.removeprefix("alsa_output.").rsplit(".", 1)[0]
                captures = [item for item in graph["captures"]
                            if item["name"].removeprefix("alsa_input.").rsplit(".", 1)[0] == stem]
                if not captures or not all(self._capture_ready(item) for item in captures):
                    output = None
            if output:
                key = ("output", mid)
                desired.add(key)
                self._route(key, mix["sink"], output, "openwave_loop_output_" + mid, 1.0, False, graph)
        for key in list(self._procs):
            if key not in desired:
                self._drop_route(key)
        self._restore_streams(graph, wanted_moves)
        with self._lock:
            removed = set(self._removed_sinks)
        for name in removed:
            if name in wanted_sinks:
                with self._lock:
                    self._removed_sinks.discard(name)
            elif name not in self._owned_sinks and self._pw.remove_mix_sink(name, graph):
                with self._lock:
                    self._removed_sinks.discard(name)
        for name, handle in list(self._owned_sinks.items()):
            if name not in wanted_sinks and self._pw.destroy_sink(handle, graph):
                del self._owned_sinks[name]

    def _restore_streams(self, graph, wanted):
        streams = {stream["serial"]: stream for stream in graph["streams"].values()}
        for serial, original in list(self._moved.items()):
            if serial in wanted:
                continue
            stream = streams.get(serial)
            if stream is None or not str(stream.get("sink", "")).startswith("openwave_src_"):
                del self._moved[serial]
                continue
            target = original if original in graph["sinks"] else graph["default"]
            if target in graph["sinks"] and not target.startswith("openwave_src_") and self._pw.move_stream(stream, target):
                del self._moved[serial]

    def _teardown(self):
        for key in list(self._procs):
            self._drop_route(key)
        try:
            graph = self._pw.snapshot()
            self._restore_streams(graph, {})
            for name, handle in list(self._owned_sinks.items()):
                if self._pw.destroy_sink(handle, graph):
                    del self._owned_sinks[name]
        except GraphError as exc:
            # No current ownership evidence means no deletion, especially after
            # a server restart. Never kill unrelated processes to compensate.
            _log.warning("Could not remove owned virtual sinks: %s", exc)
