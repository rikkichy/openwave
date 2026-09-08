"""Worker-owned PipeWire routing. Public reads are detached cached snapshots.

A configured application owns an intake even with all sends at zero: its audio
cannot bypass a zero fader. One deterministic claim and one move replace copying
an application's already-playing output. Graph observations, not child liveness,
determine whether links and levels need repair.
"""

import atexit
import copy
import ctypes
import json
import logging
import os
import signal
import subprocess
import threading
import uuid

from . import sources

_log = logging.getLogger(__name__)
CONFIG_PATH = os.path.expanduser("~/.config/openwave/mixes.json")
MIX_SINKS = {"personal": "openwave_personal_mix", "chat": "openwave_chat_mix", "record": "openwave_record_mix"}
CARD_NAME_TOKENS = ("Elgato_Wave_", "Elgato_XLR_Dock")
try:
    _libc = ctypes.CDLL(None, use_errno=True)
except OSError:
    _libc = None


def _set_pdeathsig():
    if _libc is not None:
        _libc.prctl(1, int(signal.SIGTERM), 0, 0, 0)
        if os.getppid() == 1:
            os.kill(os.getpid(), signal.SIGTERM)


def properties(values):
    """SPA property object; JSON quoting also escapes untrusted display labels."""
    return "{ " + " ".join(f"{key} = {json.dumps(value, ensure_ascii=True, allow_nan=False)}" for key, value in values.items()) + " }"



def _is_wave_card(node_name):
    return any(token in node_name for token in CARD_NAME_TOKENS)

def source_sink_name(source_id):
    return "openwave_src_" + sources.safe_id(source_id)


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
            if not isinstance(objects, list) or any(not isinstance(items, list) for items in pulse.values()):
                raise ValueError("Expected graph arrays")
            default = self.run(["pactl", "get-default-sink"]).strip()
            nodes = {}
            ports = {}
            links = set()
            for obj in objects:
                info = obj.get("info") or {}
                props = info.get("props") or {}
                typ = obj.get("type", "").rsplit(":", 1)[-1]
                if typ == "Node":
                    name = props.get("node.name", "")
                    nodes[name] = {"id": obj["id"], "serial": str(props.get("object.serial", obj["id"])), "props": props}
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
                    "identity": node["serial"], "volume": max(volumes, default=1.0), "muted": bool(sink.get("mute")),
                    "priority": int(node["props"].get("priority.session", 0))}
            sink_indices = {str(sink["index"]): name for name, sink in sinks.items()}
            inputs = {str(item.get("properties", {}).get("object.serial", "")): item for item in pulse["sink-inputs"]}
            streams = {}
            captures = []
            for name, node in nodes.items():
                props = node["props"]
                media_class = props.get("media.class", "")
                if media_class == "Stream/Output/Audio" and not name.startswith("openwave_"):
                    pulse_input = inputs.get(node["serial"], {})
                    streams[node["id"]] = {"id": node["id"], "serial": node["serial"],
                        "pulse_id": pulse_input.get("index"), "sink": sink_indices.get(str(pulse_input.get("sink"))),
                        "app_name": props.get("application.name") or name, "node_name": name,
                        "media_name": props.get("media.name", ""), "binary": props.get("application.process.binary", "")}
                if media_class == "Audio/Source" and not name.startswith("openwave_") and not name.endswith(".monitor"):
                    captures.append({"name": name, "description": props.get("node.description", name), "priority": int(props.get("priority.session", 0))})
            mutes = {item["name"]: bool(item.get("mute")) for item in pulse["sources"]}
            return {"nodes": nodes, "ports": ports, "links": links, "sinks": sinks, "streams": streams,
                    "captures": captures, "capture_mutes": mutes, "modules": pulse["modules"], "default": default}
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

    def create_sink(self, name, description):
        token = uuid.uuid4().hex
        props = properties({"node.description": description, "media.name": name, "openwave.owner": token,
                            "priority.session": 0, "monitor.channel-volumes": True})
        try:
            module_id = int(self.run(["pactl", "load-module", "module-null-sink", "sink_name=" + name,
                                      "channels=2", "channel_map=front-left,front-right", "sink_properties=" + props]).strip())
        except (GraphError, ValueError):
            return None
        return (module_id, token)

    def destroy_sink(self, handle, graph):
        module_id, token = handle
        owned = any(str(module.get("index")) == str(module_id) and token in str(module.get("argument", "")) for module in graph["modules"])
        return not owned or self.command(["pactl", "unload-module", str(module_id)])

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
            return subprocess.Popen(["pw-loopback", "--capture-props=" + properties(capture),
                                     "--playback-props=" + properties(playback)], stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL, preexec_fn=_set_pdeathsig)
        except OSError:
            return None


class Mixer:
    """Desired state setters enqueue work; stop is a synchronous drain barrier.

    A failed discovery leaves the last snapshot intact. Failed moves, links and
    levels are retried every poll; no success is inferred from a living child.
    """

    def __init__(self, pw=None, *, config_path=None, poll_interval=1.0):
        self._pw = pw or SubprocessPipeWire()
        self._path = config_path or CONFIG_PATH
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
        self._sources = {}
        self._mixes = {mid: {"id": mid, "sink": sink, "name": mid.title() + " Mix", "description": "OpenWave " + mid.title() + " Mix"} for mid, sink in MIX_SINKS.items()}
        self._graph = None
        self._error = None
        self._procs = {}
        self._owned_sinks = {}
        self._moved = {}
        self.mic = self.hp = None
        self._reported_streams = set()
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

    def poll_streams(self):
        """Compatibility for current UI; deltas concern cached observations only."""
        self.request_stream_poll()
        current = set(self.streams())
        added, removed = current - self._reported_streams, self._reported_streams - current
        self._reported_streams = current
        return added, removed

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
        handle = self._owned_sinks.get(name)
        if handle is not None:
            if any(str(module.get("index")) == str(handle[0]) and handle[1] in str(module.get("argument", "")) for module in graph["modules"]):
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
            self._procs[key] = {"proc": proc, "spec": spec, "name": name}
            return False
        node = graph["nodes"].get(name)
        if node is None or not self._pw.set_level(node["id"], volume, muted):
            return False
        # Establish gain before audio can enter a newly created stream.
        playback_ok = publish or self._link_nodes(graph, name, target)
        capture_ok = self._link_nodes(graph, source, name + "_cap") if playback_ok else False
        return playback_ok and capture_ok

    def _reconcile(self, graph):
        with self._lock:
            records, mixes, state = copy.deepcopy((self._sources, self._mixes, self._state))
        # Until the UI owns its mic row, retain the established built-in id.
        if "mic" not in records and self.mic:
            records["mic"] = {"id": "mic", "kind": "device", "node_name": self.mic, "name": "Microphone", "level": 1.0}
        wanted_sinks = {mix["sink"] for mix in mixes.values()}
        ready = {mid for mid, mix in mixes.items() if self._ensure_sink(mix["sink"], mix["description"], graph)}
        claims = claim_streams(records, graph["streams"])
        desired = set()
        wanted_moves = {}
        for sid, source in records.items():
            if sources.kind(source) == sources.KIND_APP:
                capture = source_sink_name(sid)
                wanted_sinks.add(capture)
                if not self._ensure_sink(capture, source["name"], graph):
                    continue
                for stream_id in claims[sid]:
                    stream = graph["streams"][stream_id]
                    wanted_moves[stream["serial"]] = capture
                    if stream["sink"] != capture:
                        if self._pw.move_stream(stream, capture):
                            self._moved.setdefault(stream["serial"], stream["sink"])
            else:
                capture = source.get("node_name") or (self.mic if sid == "mic" else None)
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
        if "personal" in ready and self.hp:
            key = ("output", "personal")
            desired.add(key)
            self._route(key, mixes["personal"]["sink"], self.hp, "openwave_loop_personal_to_hp", 1.0, False, graph)
        for key in list(self._procs):
            if key not in desired:
                self._drop_route(key)
        wanted_sinks.update(self._restore_streams(graph, wanted_moves))
        for name, handle in list(self._owned_sinks.items()):
            if name not in wanted_sinks and self._pw.destroy_sink(handle, graph):
                del self._owned_sinks[name]

    def _restore_streams(self, graph, wanted):
        streams = {stream["serial"]: stream for stream in graph["streams"].values()}
        retained = set()
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
            else:
                retained.add(stream["sink"])
        return retained

    def _teardown(self):
        for key in list(self._procs):
            self._drop_route(key)
        try:
            for _ in range(3):
                graph = self._pw.snapshot()
                retained = self._restore_streams(graph, {})
                if not retained:
                    break
            for name, handle in list(self._owned_sinks.items()):
                if name not in retained and self._pw.destroy_sink(handle, graph):
                    del self._owned_sinks[name]
        except GraphError as exc:
            # No current ownership evidence means no deletion, especially after
            # a server restart. Never kill unrelated processes to compensate.
            _log.warning("Could not remove owned virtual sinks: %s", exc)
