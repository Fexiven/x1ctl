# x1ctl — Sound Blaster X1 on Linux

Control the on-device features of the **Creative Sound Blaster X1** (USB `041e:3286`) from
Linux: Super X-Fi, the 10-band graphic EQ with Creative's official presets, headphone
and mic level/mute — with live events from the SXFI button and volume knob.

Audio itself already works through ALSA/PipeWire (the X1 is a standard UAC2 device); this
talks to its separate CDC control port (`/dev/ttyACM*`).

```
$ x1ctl status
Device:    Sound Blaster X1 (/dev/ttyACM0), firmware 1.0.220525.1100, serial W6ZZ0090…
Super X-Fi: on
EQ:        on   preamp +0.0 dB
    Hz     31    62   125   250   500    1k    2k    4k    8k   16k
    dB   +2.0  +2.0  -2.0  -2.0  +0.0  +0.0  +2.0  -2.0  +0.0  +0.0
EQ slot:   1 (device profile; follows Super X-Fi)
Headphone: -12.0 dB, mute off
```

## Components

| | |
|---|---|
| `x1d` | user daemon: owns the port, unlocks the device, reconnects on hotplug/resume, publishes everything on D-Bus (`io.github.x1ctl`) |
| `x1ctl` | CLI; uses `x1d` when running, otherwise opens the device itself |
| `x1-gui` | control window (Super X-Fi, 10-band EQ + preamp, presets, level/mute) |
| `x1-gui --tray` | tray icon: menu with Super X-Fi / EQ / presets / mute, middle-click toggles Super X-Fi, scroll changes level |
| `packaging/` | udev rule, systemd user unit, D-Bus activation file |
| `flake.nix` | Nix package + NixOS module |
| `docs/PROTOCOL.md` | the protocol, as recovered from Creative App, the product package and the firmware |

## Usage

```sh
x1ctl status [--json]
x1ctl sxfi [on|off|toggle]
x1ctl eq [show|on|off]
x1ctl eq set 1k -3.5            # band by index 0-9 or by frequency
x1ctl eq preamp -2
x1ctl eq bands 2 2 -2 -2 0 0 2 -2 0 0 --preamp 0
x1ctl eq import [ZZ0090.zip]     # get Creative's 71 official presets (download or local zip)
x1ctl eq presets                 # Flat + Creative's presets + yours
x1ctl eq preset counter strike 2 # loose matching; sound-mode names work too
x1ctl eq modes                   # Creative's sound modes and their EQ preset
x1ctl eq save my-preset          # → ~/.config/x1ctl/presets.json
x1ctl volume [-20 | -r 2 | -r -- -3]
x1ctl mute [on|off|toggle]
x1ctl monitor                    # live events
x1ctl sxfi-info                  # Super X-Fi capabilities + stored headphone profile
x1ctl info                       # serial, firmware build, chip, USB link, capabilities
x1ctl factory-reset --yes        # Creative's "reset to defaults" (if the X1 reports support)
x1ctl uac2 [on|off --force]      # device's EnableUAC2 flag (not needed on Linux)
```

The X1 keeps **two EQ settings**, one for Super X-Fi on and one for off; EQ writes go to
whichever is active, and toggling Super X-Fi swaps them. Settings persist on the device.

Creative's official presets aren't shipped with x1ctl. Import them once from Creative's
download (`x1ctl eq import`, or the "Import Creative presets" button in the window); if the
download is blocked, fetch [ZZ0090.zip](http://files.creative.com/creative/bin/apps/swureleases/win/creativeapp/release/product/ZZ0090/1.0.05.00/ZZ0090.zip)
in a browser and run `x1ctl eq import ~/Downloads/ZZ0090.zip`.

Creative's "sound modes" (Gaming, FPS, Valorant, …) combine an EQ preset with Acoustic
Engine effects (surround, crystalizer, bass, smart volume, dialog+). Those effects run on
Windows, not in the X1, so here a sound mode applies its EQ only. For the rest use
EasyEffects or a PipeWire filter-chain.

### GUI

`x1-gui` opens the control window; launching it again raises the existing window.
`x1-gui --tray` runs the tray icon (left click opens the window, middle click toggles Super
X-Fi, mouse wheel steps the headphone level, right click for the menu). Both are clients of
`x1d` and follow the device live — the SXFI button, the volume knob and other clients.
User presets saved from the window land in the same `presets.json` the CLI uses.

The GUI is behind the `gui` cargo feature (`cargo build --release --features gui`); the
Nix package always includes it and wraps it with the GL/Wayland/X11 libraries it loads.

### D-Bus

Object `/io/github/x1ctl/Device`, interface `io.github.x1ctl.Device`.
Properties (with change notifications): `Connected`, `Port`, `Firmware`, `Serial`,
`SuperXFi`, `EqEnabled`, `Preamp`, `Bands` (ad), `BandFrequencies`, `EqProfile`, `Volume`,
`Muted`, `MicLevel`, `MicMuted`, `UAC2Forced`, `StateJSON`.
Methods: `SetSuperXFi(b)`, `ToggleSuperXFi() → b`, `SetEqEnabled(b)`, `SetPreamp(d)`,
`SetBand(y, d)`, `SetBands(ad, d)`, `ApplyPreset(s)`, `ListPresets() → as`, `SetVolume(d)`,
`SetMuted(b)`, `SetUAC2Forced(b)`, `HeadphoneProfile() → (ss)`, `FactoryReset()`, `Refresh()`.

KDE global shortcut for the SXFI toggle:
`busctl --user call io.github.x1ctl /io/github/x1ctl/Device io.github.x1ctl.Device ToggleSuperXFi`

## Install

### NixOS (flake)

```nix
# flake.nix inputs
x1ctl.url = "path:/path/to/x1ctl";   # or a git URL
# in your nixosSystem modules
x1ctl.nixosModules.default
{ services.x1ctl.enable = true; }
```
This installs the binaries, the udev rule (uaccess + ModemManager exclusion + hotplug
start of `x1d`), the `x1d` user service, the "X1 Control" app-menu entry and the tray icon
(started with the graphical session; `services.x1ctl.tray = false;` to disable). `nix build` / `nix run .#x1ctl -- status` work too.

### Other distributions

```sh
cargo build --release
sudo install -m755 target/release/x1d target/release/x1ctl /usr/local/bin/
sudo install -m644 packaging/70-sound-blaster-x1.rules /etc/udev/rules.d/
sudo sed 's|/usr/bin/|/usr/local/bin/|' packaging/x1d.service | sudo tee /etc/systemd/user/x1d.service >/dev/null
sudo udevadm control --reload && sudo udevadm trigger
systemctl --user daemon-reload && systemctl --user enable --now x1d
```

## Safety

Every outgoing frame passes an allowlist (`proto::check_outgoing`): only the X1 commands
documented in `docs/PROTOCOL.md`, with known payloads. Firmware upgrade (`53`), debug
memory (`54`), factory reset (`9B`), operation-mode changes and all `5B` frames cannot be
sent. EQ values are clamped to the device ranges (bands ±9 dB, preamp ±12 dB). If the
device ever gets into an odd state, unplug and replug it: that resets the session; EQ and
Super X-Fi settings are kept.

## Testing without hardware

`tools/fake_x1.py <path>` creates a simulated X1 on a pty (real auth, real frame
sequences; `SIGUSR1` = SXFI button, `SIGUSR2` = knob click):

```sh
python3 tools/fake_x1.py /tmp/x1 & x1ctl --direct --port /tmp/x1 status
```
`tools/x1-hwtest.sh` is the hardware test that logs every frame for diagnosis.

## Credits

The unlock scheme is shared with the Katana V2/V2X; `src/auth.rs` is adapted from the
MIT-licensed [`v2x`](https://crates.io/crates/v2x) crate by Rasmus Moorats.

Creative's presets are not part of this repository; `x1ctl eq import` reads them from
Creative's own download on your machine. Not affiliated with or endorsed by Creative
Technology. Sound Blaster and Super X-Fi are trademarks of Creative Technology Ltd.
