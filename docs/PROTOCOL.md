# Sound Blaster X1 — control protocol (v2)

Device: Creative Sound Blaster X1, USB `041e:3286`, Creative package/model `ZZ0090`, internal
codename **Allion** ("Creative Allion One"). Firmware tested: `1.0.220525.1100`.

Sources, in order of authority:

1. **Creative App 1.24.05** — command names, enum values and payload layouts as used by
   Creative's own Windows software for this device (the "CDC raw" command set).
2. **Creative's X1 product package** `ZZ0090.zip` v1.0.05.00 (the public download Creative
   App fetches at runtime).
3. **The public X1 firmware image** `ZZ0090_fc00_2032E560.zif`.
4. USB captures of Creative App talking to an X1, and live probing on Linux.

These notes exist for interoperability: so the X1's own on-device features can be used from
Linux. No Creative code or data is included in this repository.

Markers: **[confirmed]** seen on the wire · **[Creative]** layout/name as used by Creative App ·
**[sim]** exercised against the simulator only, awaiting hardware confirmation.

---

## 1. What the X1 does on board

| Feature | On the X1? | Notes |
|---|---|---|
| Super X-Fi on/off | yes | `26` HardwareButton, button `0x1E` |
| Graphic EQ, 10 bands + preamp | yes | `11`/`12`, module `0x96`, ids `09…14` |
| Separate EQ per Super X-Fi state | yes | confirmed; selected by the Super X-Fi state |
| Headphone level / mute | yes | `23`/`24`, audio control index 0 [Creative] |
| Mic level / mute | yes | audio control index 1 [Creative] |
| EnableUAC2 flag | yes | `39` FeatureControl bit 26 |
| Super X-Fi headphone profile (HRTF) upload | yes | native raw-data transfer, out of scope |
| Surround, Crystalizer, Bass, Smart Volume, Dialog+ | **no** | Acoustic Engine runs on Windows |
| CrystalVoice, VoiceFX, Mic EQ, NoiseClean | **no** | Windows-side |
| Scout Mode, lighting, custom buttons, Bluetooth | **no** | |

Confirmed by the device's own support masks (`10` → `GraphicEQ` only, `39/02` →
`EnableUAC2` only) and by every non-X1 opcode answering `81 not supported`.

## 2. Session

Unchanged from v1 and confirmed by the firmware strings:

```
→ 5A 03 00 0D 0A                 probe; binary mode answers 5A 03 02 00 02
→ "whoareyou\r\n"                ← "whoareyou" 1E 04 86 32 nonce[32] "\r\n"
→ "unlock" R[64] "\r\n"          ← "unlock_OK\r\n"
→ "SW_MODE1\r\n"                 ← 5B 02 0A 00 6D 00…   (binary from here until power-cycle)
```

`R` = 16-byte IV (first 12 bytes are the GCM nonce) ‖ AES-256-GCM(nonce) ‖ tag, key =
`1E 04 ‖ 28 static bytes from CTCDC.dll ‖ 86 32`. Note that the **nonce is binary and can
contain `\r\n`**: read the challenge by length (45 bytes after "whoareyou" start), not by line.

The firmware's text console (after unlock) also knows `ver` ("Show the firmware and
hardware version"), `qspi` and an XON/XOFF toggle; `sw_mode1` "Switch to Creative Protocol
Mode, disabling debugger". None are needed.

## 3. Framing

`5A <op> <len> <payload>` · ack `5A 02 0A <op> <status> <8 bytes>` · `5B <sub> <len16> …`
(received only). Status codes [Creative `ACKStatusCode`]: `00` success, `01` data pending,
`02` conditional success, `80` failure, `81` not supported, `82` temporarily unsupported,
`83` invalid parameter, `84` invalid length, `85` booting.

## 4. Commands — corrections and additions to v1

### `09` DeviceInformationV2 — **request payload is the sub-op only** [confirmed]
| Request | Reply payload | Meaning [Creative] |
|---|---|---|
| `5A 09 01 00` | `00 86 32 00 01 1E 04 …` | General: PID, version, VID |
| `5A 09 02 01 00` | `01 00 60 E5 32 20 2D 04` | Firmware: index 0, **version `0x2032E560`**, MCU type `0x2D`, format 4 |
| `5A 09 01 02` | `02 10 "1.0.220525.1100" 00` | FirmwareVersionString |
| `5A 09 01 03` | `03 "W6ZZ0090…" 00` | SerialNumberString |

`0x2032E560` and MCU `0x2D` (= 45) are exactly the `version` and `chipId` in Creative's
firmware metadata for the X1 (§7).

### `26` HardwareButton [Creative layouts]
* **Support** `5A 26 01 05` → `Operation(05) Flag NumButtons StartIndex u16[NumButtons]`
  = `05 00 04 00 | 1E 00 | 0A 40 | 0B 40 | 0C 40` (12 bytes; the v1 transcription had dropped
  the `StartIndex` byte, which is why its length didn't add up).
* **Get** `5A 26 03 08 FF FF` → `Operation(08) ButtonId:u16(FFFF=all) ButtonMask:u64`.
  Bit *n* of the mask is the state of the *n*-th button in the support list; Super X-Fi is
  entry 0, so `…FF FF 01 00…` = on.
* **Set** `5A 26 05 07 <id:u16> <value:u16>` → `07 1E 00 01 00` = Super X-Fi on.

### `39` FeatureControl [Creative `FeatureControlOperation`, `FeatureMask`]
| Op | Request | Reply |
|---|---|---|
| Support | `5A 39 01 02` | `02 <supported:u32> <ResetNeeded:u32>` → `00000004 / 0` |
| Get | `5A 39 05 01 <mask:u32>` | `01 <enabled:u32>` |
| Set | `5A 39 03 00 <bit> <0|1>` | ack |

**Correction:** the v1 "write" `5A 39 05 01 00 00 00 04` is actually a **Get** of
EnableUAC2, and the X1's answer `01 00 00 00 00` means the flag is currently **off**. The
second u32 of the support reply is `ResetNeededMask`, not a "forced-on" mask. Creative App
writes it with `5A 39 03 00 1A 01` (bit 26); the device re-enumerates. Linux doesn't need it.

### `21`/`22`/`23`/`24` audio controls [Creative; confirmed on hardware]
* `5A 21 01 00` → `Total Count Additional StartIndex u16[Count]`. X1: `02 02 00 00 C2 09 C3 0A`.
  Each u16: bits 0–5 type, `0x40` has volume, `0x80` has mute, `0x100` source,
  `0x200` recording, `0x400` custom name, `0x800` dB scale. → index 0 = **headphone**
  (type 2, volume+mute, dB), index 1 = **mic** (type 3, recording, volume+mute, dB).
* `5A 22 21 <count> <idx[32]>` → `Count Additional {idx max:i16 min:i16 step:u16}…`, 1/256 dB.
  X1 says headphone −49.8…0 dB step 1.77, mic −48…0 dB step 1. In practice the headphone
  level moves in **440-unit steps (1.71875 dB)**: knob events read −6.88, −8.59, −10.31,
  −12.03 dB, and min −12760 = −29 × 440. A set to an in-between value is rounded *up* to the
  next step (−12.0 → −10.31), so x1ctl snaps to the nearest step and reads back.
  The reported max (0 dB) is wrong. The real maximum is **+2200 = +8.59 dB**; values above it
  are not rejected — the X1 goes silent [confirmed]. x1ctl's allowlist refuses them.
* Level get `5A 23 02 01 <idx>` → `<idx> <i16>` (`5A 23 03 00 68 F7` = −8.59 dB);
  set `5A 23 04 00 <idx> <i16>` → ack;
  unsolicited `5A 23 03 <idx> <i16>` on knob turns.
* Mute get `5A 24 02 01 <idx>` → `<idx> <0|1>`; set `5A 24 03 00 <idx> <0|1>`.

### `1A` ActiveMalcolmProfile / `1D` MalcolmProfileInformation [Creative]
* Get `5A 1A 03 01 02 00` (Get, type Device=2) → `<type> <index>`; also sent unsolicited
  (`5A 1A 02 02 xx`) when Super X-Fi toggles. This is the X1's per-Super-X-Fi EQ slot.
  Set exists (`00 02 <index>`) but is not used. On hardware the index stays 0 through every
  Super X-Fi toggle — yet the X1 **does keep two independent EQs** [confirmed: 31 Hz set to −6
  with Super X-Fi on, still +4 after switching it off]. The active EQ follows the Super X-Fi
  state; the profile index is not the selector.
* `5A 1D 03 01 02 00` = profile information for type Device.

### `6F` SuperXFi [Creative]
* `5A 6F 01 00` → `00 RRMetaVer RRSupportSize RRChannelSupport HPCMetaVer HPCSupportSize HPCFormatSupport`
  = room-response v3 channels `0x07`, headphone-compensation v2 formats `0x1C`.
* `5A 6F 01 06` → active headphone info: HPID and FileID strings. With no personalised
  profile installed the X1 answers `06` + 64 zero bytes [confirmed].
* `5A 6F 02 05 xx` (unsolicited): **Creative App ignores op 5** — no handler exists.

### `9B` FactoryReset [Creative; support on the X1 unconfirmed]
Ops: `01` SWFactoryReset, `02` GetFactoryResetSupport, `03` FullFactoryReset. For CDC devices
Creative App's Device settings → Reset calls `CTCDCCMD_FactoryReset` with type **General (1)**,
i.e. `5A 9B 01 01`. x1ctl queries `5A 9B 01 02` first and only enables the reset if the X1
doesn't NAK it; `03` (Full) is never sent.

### `6D` DeviceOperationMode
`5A 6D 01 01` → `01 00` = Normal. Set would switch to Debug/Production/Recovery: **never**.

## 5. Opcode map (Creative `CDCRawCommand`)

`02` Ack · `03` MaxPayloadSize · `07`/`09` DeviceInformation V1/V2 · `0C` ConnectionMode ·
`0E` ConnectionStatus · `10` SubFeatureSupport · `11`/`12` Get/SetMalcolmParameter ·
`17` ProfileData · `18` ProfileNameData · `1A` ActiveProfile · `1D` ProfileInformation ·
`21` AudioControlInfo · `22` LevelRanges · `23` AudioLevel · `24` AudioMute · `26` HardwareButton ·
`27` BatteryLevel · `29` SpeakerConfiguration · `2C` OutputTargetSelection · `2D` MicPriority ·
`31` BTAutoConnect · `39` FeatureControl · `3A` LEDControl · `40` SubwooferSetup ·
`42` BTFeatureControl · `44` GraphicEqualizerControl · `48` AudioPrompt · `49` DeviceColor ·
`4A` SuperWide · `4B` SpeakerChannelConfig · `53` **Upgrade** · `60` BTSinkControl ·
`6B` BTHPControl · `6C` AudioFilter · `6D` OperationMode · `6F` SuperXFi ·
`76` WirelessAudio · `80` NetworkStandby · `81` LEDSegmentBrightness · `96` DeviceCallback ·
`9B` **FactoryReset** · `9C` DeviceModeControl · `A5` CustomButton · `A7` SoundModeControl ·
`A8` ModuleProfileControl · `A9` Passthrough.

x1ctl's allowlist permits only the X1 commands above with known payloads; `53`, `54`, `9B`,
`6D` set, all `5B` frames, and everything else are refused before reaching the port.

## 6. EQ presets and sound modes (product package `ZZ0090` v1.0.05.00)

71 factory EQ presets (`Presets/EQ/*.json`, all preamp 0, all within ±9 dB) and 60 sound modes
(`SoundMode/*.json`). Each sound mode = Acoustic Engine settings (host-side) + an EQ preset
id + `SuperXFi.Mode` (always 0). On the X1 only the EQ part has an effect. The 8 band sets
captured in round 1 are: Gaming, Music, Movies, 64 Audio Aspire Series, Adventure and Action,
Apex Legends, Arena Of Valor, Fortnite. The package also carries Mic EQ / VoiceFX / NoiseClean
profiles — all for host-side processing. x1ctl doesn't redistribute any of it: `x1ctl eq import`
reads the presets from Creative's own download on the user's machine.

## 7. Firmware

* Update check: `http://files.creative.com/creative/bin/firmware/fwureleases/ZZ0090/ZZ0090_release.json`
  → `latestVersion 1.0.220525.1100` (current units are up to date).
* Metadata: `…/fwureleases/ZZ0090/1.0.220525.1100/ZZ0090_metadata.json` → module `fc00`,
  chip 45, version `2032E560`, 1 273 212 bytes, checksum `ba8e5611` (= CRC-32 of the file).
* Image: `…/fwureleases/ZZ0090/zifs/fc00/ZZ0090_fc00_2032E560.zif`.
* Container: RIFF-style **`CIFF`** with chunks `CINF` ("Creative Allion One"), `CIN2`,
  `EXEA` (512 B), `DATA` (1 220 792 B), `DATA` (51 712 B), `CHK2` (32 B, likely a SHA-256).
  Not encrypted: ARM code for a **Cortex-A5** running **FreeRTOS 8.2.3**, Opus voice prompts
  (`superxfi_config_amp.opus`), version string format `SDK/BOOT/MCU/DSP`.
* Firmware strings name Super X-Fi modes `HEADPHONE`, `SPEAKER`, `INPERSON MIC`, `BATTLE`,
  `MUSIC CALL` (shared SDK); Creative App only ever sends mode 0 for the X1.
* Flashing goes through `53` Upgrade + `5B` transfers. **Not implemented, deliberately.**

## 8. Open items

| Item | How to resolve |
|---|---|
| `6F 05 xx` meaning | listen while plugging headphones/mic, playing/stopping audio |
| `1D` reply fields | not needed |
