#!/usr/bin/env bash
# Actual installed OpenWave, private audio and real GTK/AT-SPI. No host daemon,
# USB, sound device, user bus, service manager, installation or settings access.
set -euo pipefail
umask 077

usage() {
    cat <<'USAGE'
Usage: smoke-install.sh --stage DIR [--output-dir DIR] [--driver FILE]
                        [--gtk-tests FILE] [--source-archive FILE --sha256 HEX]

DIR is either an installed prefix or a DESTDIR containing usr/bin/openwave.
Build the external driver beforehand (not during a concurrent source edit):
  cargo build --locked -p openwave-runtime --example smoke-control
The optional --gtk-tests is the compiled openwave-desktop library test binary.
Its ignored display-requiring cases run separately on the private Xvfb/bus.
Run this finite smoke under the harness process supervisor. Its PID namespace
owns and reaps every child; no host X/audio/session socket is bound. Evidence,
private state and logs remain in --output-dir (must be empty). The optional
archive/digest pair ties installed supplied artwork/CSS/VERSION to the single
prepared release source archive. No package publication or build occurs here.

Requires bubblewrap with user/PID namespaces, Xvfb, D-Bus/AT-SPI activation,
xdotool, ImageMagick import, jq, PipeWire/Pulse clients (including pacat), and the
runtime audio helpers. A missing capability is a FAILURE, never a skipped pass.
If namespaces are denied, run in a disposable VM/container with these namespace
capabilities and no host devices/sockets; there is no unsafe host-mode fallback.
USAGE
}

if [[ ${1-} != --inside && ${1-} != --session ]]; then
    stage= output= driver= gtk_tests= archive= digest=
    while (($#)); do
        case $1 in
            --stage|--output-dir|--driver|--gtk-tests|--source-archive|--sha256)
                (($# >= 2)) || { usage >&2; exit 2; }
                case $1 in --stage) stage=$2;; --output-dir) output=$2;; --driver) driver=$2;; --gtk-tests) gtk_tests=$2;; --source-archive) archive=$2;; --sha256) digest=$2;; esac
                shift 2;;
            --help|-h) usage; exit 0;;
            *) echo "Unknown smoke argument: $1" >&2; usage >&2; exit 2;;
        esac
    done
    [[ -n $stage ]] || { usage >&2; exit 2; }
    [[ $(id -u) != 0 ]] || { echo 'Run smoke as an ordinary user in an isolated builder, not root.' >&2; exit 1; }
    stage=$(realpath "$stage")
    if [[ -x $stage/bin/openwave ]]; then prefix=$stage
    elif [[ -x $stage/usr/bin/openwave ]]; then prefix=$stage/usr
    else echo "No installed openwave in $stage/bin or $stage/usr/bin" >&2; exit 1; fi
    source_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
    if [[ -z $driver ]]; then
        for candidate in "${CARGO_TARGET_DIR:-$source_root/target}/debug/examples/smoke-control" "${CARGO_TARGET_DIR:-$source_root/target}/release/examples/smoke-control"; do
            if [[ -x $candidate ]]; then driver=$candidate; break; fi
        done
    fi
    [[ -n $driver && -x $driver ]] || { echo 'Missing built smoke-control example; use --driver FILE (see --help).' >&2; exit 1; }
    driver=$(realpath "$driver")
    if [[ -n $gtk_tests ]]; then
        [[ -f $gtk_tests && -x $gtk_tests ]] || { echo 'Missing compiled desktop GTK tests.' >&2; exit 1; }
        gtk_tests=$(realpath "$gtk_tests")
    fi
    [[ -n $output ]] || output=$(mktemp -d "${TMPDIR:-/tmp}/openwave-smoke.XXXXXXXX")
    mkdir -p -- "$output"
    output=$(realpath "$output")
    [[ -z $(printf '%s\n' "$output"/{*,.[!.]*,..?*} | while IFS= read -r p; do [[ ! -e $p && ! -L $p ]] || printf '%s\n' "$p"; done) ]] || { echo 'Smoke output directory must be empty.' >&2; exit 1; }
    chmod 700 "$output"
    [[ $output != "$prefix" && $output != "$prefix/"* ]] || { echo 'Output must not be inside the installation.' >&2; exit 1; }
    mkdir -p "$output"/{home,config,data,state,cache,run,logs,evidence}
    chmod 700 "$output/run"
    if [[ -n $archive || -n $digest ]]; then
        [[ -f $archive && $digest =~ ^[a-fA-F0-9]{64}$ ]] || { echo 'Archive requires a SHA256 digest.' >&2; exit 2; }
        archive=$(realpath "$archive")
        actual=$(sha256sum "$archive"); actual=${actual%% *}
        [[ ${actual,,} == ${digest,,} ]] || { echo 'Source archive digest mismatch.' >&2; exit 1; }
        version=$(<"$prefix/share/openwave/VERSION")
        [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || { echo 'Invalid installed VERSION.' >&2; exit 1; }
        mkdir "$output/archive-proof"
        for name in VERSION data/style.css icons/openwave.svg icons/openwave-white.svg icons/openwave-black.svg icons/openwave-red.svg; do
            mkdir -p "$output/archive-proof/$(dirname "$name")"
            tar -xOf "$archive" "openwave-$version/$name" > "$output/archive-proof/$name"
            case $name in data/style.css) installed=style.css;; *) installed=$name;; esac
            cmp "$output/archive-proof/$name" "$prefix/share/openwave/$installed"
        done
        printf '%s  %s\n' "$actual" "$(basename "$archive")" > "$output/evidence/source-archive.sha256"
    fi
    # Expose required executables, not their entire parent directories: doing
    # the latter advertises host service-manager tools in a manager-free fixture.
    # Nix wrappers and their store dependencies are available read-only; neither
    # /run/current-system nor a home/profile directory is exposed to the sandbox.
    mkdir -p "$output/bin"
    for tool in bash env dbus-run-session dbus-daemon dbus-uuidgen Xvfb xdotool import jq pipewire pipewire-pulse wireplumber pw-dump pw-cli pw-link pw-loopback pw-cat pw-top pactl pacat wpctl amixer aplay timeout sleep mkdir chmod cat cp cmp rm sha256sum date tee; do
        executable=$(type -P "$tool") || { echo "Missing smoke capability: $tool" >&2; exit 1; }
        executable=$(realpath "$executable")
        ln -s -- "$executable" "$output/bin/$tool"
    done
    command -v bwrap >/dev/null || { echo 'Missing bubblewrap; use a disposable namespace-capable VM/container.' >&2; exit 1; }
    dbus-uuidgen > "$output/machine-id"
    bwrap_args=(--unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts --die-with-parent --new-session --cap-drop ALL --clearenv)
    for root in /nix/store /usr /bin /sbin /lib /lib64; do
        [[ ! -e $root ]] || bwrap_args+=(--ro-bind "$root" "$root")
    done
    bwrap_args+=(--dir /etc)
    for resource in /etc/fonts /etc/ld.so.cache /etc/ld.so.conf /etc/ld.so.conf.d /etc/passwd /etc/group; do
        [[ ! -e $resource ]] || bwrap_args+=(--ro-bind "$resource" "$resource")
    done
    bwrap_args+=(--ro-bind "$output/machine-id" /etc/machine-id --proc /proc)
    # ALSA proc entries are kernel-global, even in a fresh PID namespace.
    [[ ! -e /proc/asound ]] || bwrap_args+=(--tmpfs /proc/asound)
    bwrap_args+=(--dev /dev --tmpfs /tmp --dir /run --dir /var --dir /var/tmp
        --bind "$output" /work --ro-bind "$prefix" /installed --ro-bind "$driver" /smoke-control
        --ro-bind "$source_root/packaging/smoke-install.sh" /smoke-install.sh
        --setenv PATH /work/bin --setenv HOME /work/home
        --setenv XDG_CONFIG_HOME /work/config --setenv XDG_DATA_HOME /work/data
        --setenv XDG_STATE_HOME /work/state --setenv XDG_CACHE_HOME /work/cache
        --setenv XDG_RUNTIME_DIR /work/run --setenv OPENWAVE_SMOKE_SANDBOX 1
        --setenv LC_ALL C --setenv GDK_BACKEND x11 --setenv GSK_RENDERER cairo
        --setenv GTK_A11Y atspi --setenv NO_AT_BRIDGE 0 --chdir /work)
    if [[ -n $gtk_tests ]]; then
        bwrap_args+=(--ro-bind "$gtk_tests" /gtk-tests
            --setenv OPENWAVE_TEST_DATA_DIR /installed/share/openwave)
    fi
    # The selected Nix AT-SPI launcher has this compile-time daemon path.
    # Expose only the resolved executable, not the host system profile.
    dbus_daemon=$(realpath "$(command -v dbus-daemon)")
    bwrap_args+=(--ro-bind "$dbus_daemon" /run/current-system/sw/bin/dbus-daemon)
    # System data only (AT-SPI/D-Bus service descriptions, schemas and icons).
    data_dirs=/usr/local/share:/usr/share
    IFS=: read -ra candidates <<< "${XDG_DATA_DIRS-}"
    for candidate in "${candidates[@]}"; do
        [[ $candidate != /nix/store/* ]] || data_dirs="$data_dirs:$candidate"
    done
    bwrap_args+=(--setenv XDG_DATA_DIRS "$data_dirs")
    for name in LADSPA_PATH GIO_EXTRA_MODULES GDK_PIXBUF_MODULE_FILE; do
        value=${!name-}
        [[ -z $value ]] || bwrap_args+=(--setenv "$name" "$value")
    done
    printf 'Evidence directory: %s\n' "$output"
    # No direct fallback: a namespace creation error must not launch anything on
    # the user's audio/session bus. bwrap's PID-1 reaper owns the whole subtree.
    if timeout --signal=TERM --kill-after=10 480 bwrap "${bwrap_args[@]}" bash /smoke-install.sh --inside 2>&1 | tee "$output/logs/session.log"; then
        cat "$output/evidence/result.txt"
        printf 'Actual GTK screenshots: %s/evidence/{matrix,settings,host-loss,host-return}.png\n' "$output"
    else
        status=$?
        cat "$output/logs/session.log" >&2
        printf 'Smoke FAILED (%s). Evidence: %s. If namespaces are denied, use a disposable namespace-capable VM/container; no host fallback is permitted.\n' "$status" "$output" >&2
        exit "$status"
    fi
    exit 0
fi

# The internal entry points are not a user-selectable host execution mode.
[[ ${OPENWAVE_SMOKE_SANDBOX-} == 1 && $HOME == /work/home && $XDG_RUNTIME_DIR == /work/run && ! -e /dev/snd && ! -e /dev/bus/usb ]] || { echo 'Private sandbox guard failed.' >&2; exit 1; }
if [[ $1 == --inside ]]; then
    # Activate only the accessibility bus required by the GTK checks. Importing
    # host service directories advertises unrelated services, including a
    # systemd manager that deliberately does not exist in this private session.
    mkdir -p /work/dbus-services
    IFS=: read -ra service_data_dirs <<< "$XDG_DATA_DIRS"
    for data_dir in "${service_data_dirs[@]}"; do
        service="$data_dir/dbus-1/services/org.a11y.Bus.service"
        if [[ -f $service ]]; then
            cp -- "$service" /work/dbus-services/org.a11y.Bus.service
            break
        fi
    done
    [[ -f /work/dbus-services/org.a11y.Bus.service ]] || { echo 'Missing AT-SPI D-Bus activation service.' >&2; exit 1; }
    cat > /work/session.conf <<'DBUS'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/work/run</listen>
  <auth>EXTERNAL</auth>
  <servicedir>/work/dbus-services</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
DBUS
    exec dbus-run-session --config-file=/work/session.conf -- bash /smoke-install.sh --session
fi

pids=()
app_pid= watcher_pid= tone_pid= pipewire_pid= pulse_pid= policy_pid= xvfb_pid=
cleanup() {
    local status=$?
    trap - EXIT INT TERM HUP
    if ((status != 0)) && [[ ${PIPEWIRE_RUNTIME_DIR-} == /work/run ]]; then
        timeout 3 pw-dump > /work/evidence/failure-graph.json 2>/dev/null || true
        timeout 3 pactl --format=json list sink-inputs > /work/evidence/failure-streams.json 2>/dev/null || true
    fi
    if ((status != 0)) && [[ ${DISPLAY-} == :99 ]]; then
        timeout 5 import -window root /work/evidence/failure-ui.png 2>/dev/null || true
    fi
    # Captured PIDs are direct unreaped children. Never kill by executable/name.
    for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
start() {
    local variable=$1 logfile=$2; shift 2
    timeout --signal=TERM --kill-after=5 420 "$@" >"/work/logs/$logfile" 2>&1 &
    local pid=$!
    pids+=("$pid")
    printf -v "$variable" '%s' "$pid"
}
stop() {
    local variable=$1 pid=${!1} next=() item
    [[ -n $pid ]] || return 0
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    for item in "${pids[@]}"; do [[ $item == "$pid" ]] || next+=("$item"); done
    pids=("${next[@]}")
    printf -v "$variable" ''
}
wait_command() {
    local count
    for ((count=0;count<150;count++)); do
        if "$@"; then return 0; fi
        sleep .2
    done
    echo "Readiness failed: $*" >&2
    return 1
}
assert_state() {
    local filter=$1
    for ((i=0;i<150;i++)); do
        if /smoke-control state snapshot > /work/evidence/snapshot.json && jq -e "$filter" /work/evidence/snapshot.json >/dev/null; then return 0; fi
        sleep .2
    done
    echo "State did not converge: $filter" >&2
    cat /work/evidence/snapshot.json >&2
    return 1
}
visible_window() { xdotool search --onlyvisible --name '^OpenWave$'; }
screenshot() { import -window root "/work/evidence/$1.png"; }

# Informational and dry-run paths execute before sandbox-mode FLATPAK_ID is set.
expected=$(</installed/share/openwave/VERSION)
for launcher in openwave openwave-daemon openwave-diag openwave-probe; do
    timeout 15 "/installed/bin/$launcher" --help > "/work/evidence/$launcher.help"
    actual=$(timeout 15 "/installed/bin/$launcher" --version)
    [[ $actual == "$launcher $expected" ]] || { echo "Wrong installed version: $actual" >&2; exit 1; }
    printf '%s\n' "$actual" > "/work/evidence/$launcher.version"
done
sha256sum /installed/bin/openwave /installed/bin/openwave-daemon /installed/bin/openwave-diag /installed/bin/openwave-probe /installed/libexec/openwave-maintenance /smoke-control > /work/evidence/executed-binaries.sha256
timeout 15 /installed/bin/openwave --uninstall --dry-run > /work/evidence/uninstall-inspection.txt
for resource in VERSION style.css pipewire/52-openwave-mixes.conf wireplumber/51-openwave-wave-xlr.conf icons/openwave.svg icons/openwave-white.svg icons/openwave-black.svg icons/openwave-red.svg install-manifest.json; do
    [[ -s /installed/share/openwave/$resource ]] || { echo "Missing installed asset: $resource" >&2; exit 1; }
done
for name in openwave openwave-white openwave-black openwave-red; do
    context=status; [[ $name != openwave ]] || context=apps
    cmp "/installed/share/openwave/icons/$name.svg" "/installed/share/icons/hicolor/scalable/$context/$name.svg"
done
cmp /installed/share/openwave/icons/openwave.svg /installed/share/doc/openwave/icons/openwave.svg
[[ -x /installed/libexec/openwave-maintenance ]]
/smoke-control fixtures /work/config/openwave

# The server has no udev, ALSA, JACK, systemd or host discovery.
cat > /work/private-pipewire.conf <<'PIPEWIRE'
context.properties = {
    core.daemon = true
    core.name = pipewire-0
    default.clock.rate = 48000
    default.clock.quantum = 256
    default.clock.min-quantum = 256
    default.clock.max-quantum = 256
    mem.warn-mlock = false
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-access args = { access.force = unrestricted } }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
context.objects = [
    { factory = metadata args = { metadata.name = default } }
    { factory = spa-node-factory args = {
        factory.name = support.node.driver
        node.name = Dummy-Driver
        node.group = pipewire.dummy
        priority.driver = 20000
    } }
]
PIPEWIRE
cat > /work/private-pulse.conf <<'PULSE'
context.properties = { log.level = 2 }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-protocol-pulse args = {
    server.address = [ "unix:/work/run/pulse/native" ]
    pulse.min.req = 256/48000
    pulse.default.req = 256/48000
    pulse.min.quantum = 256/48000
} } ]
PULSE
export PIPEWIRE_RUNTIME_DIR=/work/run PIPEWIRE_REMOTE=pipewire-0 PULSE_SERVER=unix:/work/run/pulse/native
mkdir -p /work/config/pipewire /work/run/pulse
# Pulse clients need session policy to negotiate ports and complete connection.
# WirePlumber's upstream policy-only profile does not load hardware monitors;
# explicitly disable discovery and host-session integrations as a second guard.
mkdir -p /work/config/wireplumber/wireplumber.conf.d
cat > /work/config/wireplumber/wireplumber.conf.d/90-private-smoke.conf <<'POLICY'
wireplumber.profiles = {
    policy = {
        hardware.audio = disabled
        hardware.bluetooth = disabled
        hardware.video-capture = disabled
        support.logind = disabled
        support.reserve-device = disabled
        support.portal-permissionstore = disabled
        script.client.access-portal = disabled
    }
}
POLICY
# Ubuntu 24.04 ships WirePlumber 0.4, whose upstream policy-only configuration
# is a separate file rather than the named profile introduced in 0.5.
policy_args=(--profile=policy)
if [[ $(wireplumber --version) == *"libwireplumber 0.4."* ]]; then
    policy_args=(--config-file=policy.conf)
fi
server_ready() { [[ -S /work/run/pipewire-0 && -S /work/run/pulse/native ]] && timeout 3 pactl info > /work/evidence/pactl-info.txt && timeout 3 pw-dump > /work/evidence/ready-graph.json; }
start_audio() {
    start pipewire_pid pipewire.log pipewire -c /work/private-pipewire.conf
    start pulse_pid pulse.log pipewire-pulse -c /work/private-pulse.conf
    wait_command server_ready
    start policy_pid policy.log wireplumber "${policy_args[@]}"
    /smoke-control fixture-output > /work/evidence/fixture-output-module.txt
    pactl set-default-sink fixture_output
    for suffix in a b; do
        pactl load-module module-null-sink "sink_name=fixture_capture_$suffix" channels=2 channel_map=front-left,front-right
        pactl load-module module-remap-source "master=fixture_capture_$suffix.monitor" "source_name=fixture_mic_$suffix" channels=2 channel_map=front-left,front-right master_channel_map=front-left,front-right remix=no
    done
    /smoke-control wait-node fixture_mic_a
    /smoke-control wait-node fixture_mic_b
    pactl set-source-mute fixture_mic_b 1
    printf '%s\n' OPENWAVE_SMOKE_AUDIO_READY
}
start_tone() {
    # A Pulse playback client is essential: the app must perform its real
    # sink-input move and not merely copy an unclaimed native stream.
    start tone_pid tone.log bash -o pipefail -c '/smoke-control tone | pacat --playback --raw --format=float32le --rate=48000 --channels=2 --client-name="Fixture Music" --stream-name="Fixture Music" --property="application.name=Fixture Music" --property="node.name=fixture_music" --device=fixture_output'
}
start_watcher() {
    rm -f /work/tray-item.json /work/watcher-ready
    start watcher_pid watcher.log /smoke-control watcher /work/tray-item.json /work/watcher-ready
    wait_command test -s /work/watcher-ready
}
start_audio
start xvfb_pid xvfb.log Xvfb :99 -screen 0 1440x1000x24 -nolisten tcp -noreset
export DISPLAY=:99
wait_command xdotool getdisplaygeometry
if [[ -x /gtk-tests ]]; then
    /gtk-tests --list --ignored --format terse > /work/evidence/gtk-tests.txt
    gtk_count=0
    while IFS= read -r test_case; do
        [[ $test_case == *": test" ]] || continue
        test_case=${test_case%: test}
        gtk_count=$((gtk_count + 1))
        timeout --signal=TERM --kill-after=3 30 /gtk-tests \
            --ignored --exact "$test_case" --test-threads=1 \
            2>&1 | tee "/work/logs/gtk-test-$gtk_count.log"
    done < /work/evidence/gtk-tests.txt
    ((gtk_count > 0)) || { echo 'No isolated GTK tests were registered.' >&2; exit 1; }
    printf 'OPENWAVE_SMOKE_GTK_TESTS_PASSED %s\n' "$gtk_count"
fi
start_watcher
# FLATPAK_ID is set ONLY for this deliberately device-free process, to test
# its matrix/panel/routing mode, not native first-run setup or Polkit success.
start app_pid openwave.log env FLATPAK_ID=com.github.openwave /installed/bin/openwave --hide
wait_command test -s /work/tray-item.json
/smoke-control tray /work/tray-item.json assert-disconnected
sleep 1
if visible_window; then echo '--hide exposed a window despite an active tray host.' >&2; exit 1; fi
/smoke-control tray /work/tray-item.json open
wait_command visible_window
screenshot matrix-startup
printf '%s\n' OPENWAVE_SMOKE_GUI_READY
start_tone
/smoke-control wait-node openwave_capture_personal
/smoke-control action set-source-level "('music', 0.5)"
/smoke-control action set-cell-level "('music', 'personal', 0.4)"
assert_state '.sources[] | select(.id == "music") | .level == 0.5'
/smoke-control assert-routes
/smoke-control record openwave_capture_personal /work/evidence/source-send.f32 0.002
/smoke-control identity openwave_capture_personal > /work/evidence/publication-before.json
screenshot matrix
/smoke-control ui dump > /work/evidence/accessibility-matrix.txt

# Standard AT-SPI Value/Action interfaces operate the actual visible widgets;
# there are deliberately no private GActions for output/master smoke shortcuts.
/smoke-control ui set 'Personal master volume' 0.5
assert_state '.volumes.personal.volume == 0.5'
/smoke-control record openwave_capture_personal /work/evidence/master-scaled.f32 0.00025
/smoke-control identity openwave_capture_personal > /work/evidence/publication-after-master.json
cmp /work/evidence/publication-before.json /work/evidence/publication-after-master.json
/smoke-control ui click 'Personal output' 'toggle button'
/smoke-control ui click 'Fixture Output'
assert_state '.outputs.personal == "fixture_output"'
/smoke-control wait-node openwave_loop_output_personal
/smoke-control ui click 'Personal output' 'toggle button'
/smoke-control ui click 'Not monitored'
assert_state '.outputs.personal == "none"'
/smoke-control action toggle-source-mute "'music'"
assert_state '.sources[] | select(.id == "music") | .muted'
/smoke-control record openwave_capture_personal /work/evidence/source-muted.f32 0
/smoke-control action toggle-source-mute "'music'"
/smoke-control action switch-group "'Mics'"
assert_state '([.sources[] | select(.group == "Mics" and .muted == false)] | length) == 1 and ([.sources[] | select(.id == "mic_b" and .muted == false)] | length) == 1'
/smoke-control action save-scene "'Smoke scene'"
wait_command bash -c '/smoke-control state scenes > /work/evidence/scenes.json && jq -e '\''to_entries | any(.value == "Smoke scene")'\'' /work/evidence/scenes.json >/dev/null'
scene=$(jq -r 'to_entries[] | select(.value == "Smoke scene") | .key' /work/evidence/scenes.json)
/smoke-control action set-cell-level "('music', 'personal', 0.2)"
assert_state '.cells["music.personal"].volume == 0.2'
/smoke-control action apply-scene "'$scene'"
assert_state '.cells["music.personal"].volume == 0.4 and .volumes.personal.volume == 0.5'
/smoke-control record openwave_capture_personal /work/evidence/scene-restored.f32 0.00025
/smoke-control break-link /work/evidence/repaired-link.json
/smoke-control record openwave_capture_personal /work/evidence/repaired-link.f32 0.00025
for mix in personal chat record; do /smoke-control action set-cell-level "('music', '$mix', 0.0)"; done
assert_state '[.cells | to_entries[] | select(.key | startswith("music.")) | .value.volume] | all(. == 0)'
/smoke-control record openwave_capture_personal /work/evidence/all-sends-zero.f32 0
/smoke-control assert-routes
/smoke-control action apply-scene "'$scene'"
assert_state '.cells["music.personal"].volume == 0.4'
/smoke-control window-action 1 settings
sleep 1
screenshot settings
/smoke-control ui dump > /work/evidence/accessibility-settings.txt
/smoke-control window-action 1 save-scene-as
sleep 1
screenshot save-scene-dialog
xdotool key Escape

# Activate GTK's actual close button. xdotool windowclose calls XDestroyWindow
# and would bypass close-request, so it is deliberately not used.
/smoke-control ui click 'Close'
wait_command bash -c '! xdotool search --onlyvisible --name "^OpenWave$"'
/smoke-control tray /work/tray-item.json open
wait_command visible_window
/smoke-control ui click 'Close'
wait_command bash -c '! xdotool search --onlyvisible --name "^OpenWave$"'
stop watcher_pid
wait_command visible_window
screenshot host-loss
start_watcher
wait_command test -s /work/tray-item.json
/smoke-control tray /work/tray-item.json assert-disconnected
/smoke-control ui click 'Close'
wait_command bash -c '! xdotool search --onlyvisible --name "^OpenWave$"'
/smoke-control activate
wait_command visible_window
screenshot host-return

# Restart ONLY our captured private server children. Snapshot state stays in
# the running app; recreated nodes must restore master attenuation before use.
/smoke-control identity openwave_capture_personal > /work/evidence/before-restart.json
stop tone_pid
stop policy_pid
stop pulse_pid
stop pipewire_pid
start_audio
start_tone
/smoke-control wait-node openwave_capture_personal
/smoke-control assert-routes
/smoke-control identity openwave_capture_personal > /work/evidence/after-restart.json
if cmp -s /work/evidence/before-restart.json /work/evidence/after-restart.json; then echo 'Private server restart did not change publication generation.' >&2; exit 1; fi
/smoke-control record openwave_capture_personal /work/evidence/restart-restored.f32 0.00025
/smoke-control action levels
/smoke-control state levels > /work/evidence/quiet-levels.json
instrument='has("src:music") and has("mix:personal") and .["src:music"] > 0'
# The restored .00025 PCM is below the preserved Python .004 meter quiet floor.
# Verify that floor, then raise only this private fixture to exercise live meters.
jq -e "$instrument and .[\"mix:personal\"] == 0" /work/evidence/quiet-levels.json >/dev/null
/smoke-control action set-source-level "('music', 1.0)"
/smoke-control ui set 'Personal master volume' 1.0
assert_state '(.sources[] | select(.id == "music") | .level == 1.0) and .volumes.personal.volume == 1.0'
/smoke-control record openwave_capture_personal /work/evidence/meter-audible.f32 0.016
/smoke-control action levels
/smoke-control state levels > /work/evidence/levels.json
jq -e 'has("src:music") and has("mix:personal") and .["src:music"] > 0 and .["mix:personal"] > 0' /work/evidence/levels.json >/dev/null
pw-dump > /work/evidence/final-graph.json
/smoke-control tray /work/tray-item.json quit
wait_command bash -c '! /smoke-control state snapshot >/dev/null 2>&1'
wait "$app_pid"
next=(); for pid in "${pids[@]}"; do [[ $pid == "$app_pid" ]] || next+=("$pid"); done; pids=("${next[@]}"); app_pid=
cat > /work/evidence/result.txt <<'RESULT'
PASS: actual installed informational commands/assets and isolated GTK/audio smoke.
Real PCM: Python-compatible 0.002/0.00025 attenuation, source mute and zero sends, scene restoration,
publication identity, owned-link repair and private-server generation restoration.
Real external GTK/GActions: master/output, trim/send/mute/group/scenes, settings,
scene dialog, close/reopen, SNI host disappearance/return and asynchronous quit.
Screenshots are actual OpenWave; visual layout acceptance remains human-reviewed.
Scope: device-free FLATPAK_ID panel/routing mode, NOT native first-run/Polkit,
physical USB, host service, package-manager or real Flatpak-package acceptance.
RESULT
