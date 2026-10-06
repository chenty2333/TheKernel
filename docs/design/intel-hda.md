# Intel HDA analog and bounded HDMI playback (2026-10-06)

## Facts, boundaries and references

N305 is `8086:54c8`, class 0403 / interface 00; subsystem `10ec:12ec` is
not a codec model identification. Midday physical testing subsequently
identified `10ec0269` (ALC269 family), headphone route 0x21 -> 0x0c -> 0x02.
The driver obtained it through generic widget enumeration, not a model guess.
Codec/controller enumeration is physically verified; PCM/native ALSA playback
and headphone waveform remain **未在硬件上验证**.

Original Rust; consulted Intel HDA 1.0a §§3–7 (PDF/text under external refs/audio),
Linux 7.2.3 `sound/hda/{controllers/intel.c,core/controller.c,common/codec.c,
codecs/generic.c,codecs/hdmi/intelhdmi.c}`. Those are the relocated paths of
the older `sound/pci/hda` files named in the request. The HDA-side HDMI pin
mapping and route handling are original Rust behavior adaptation; the GPL HDA
sources are consulted for facts only, with no GPL implementation body ported.
QEMU's CORB/RIRB response-count behavior was checked against
<https://github.com/qemu/qemu/blob/master/hw/audio/intel-hda.c>.

## Native ALSA and retained OSS

`/dev/snd/controlC0` and `/dev/snd/pcmC0D0p` now expose the native Linux
x86_64 ALSA control/PCM UAPI, not an ALSA-to-OSS userspace adapter. The same
bounded playback owner serves HDA or the boot-selected VirtIO sound backend;
`/dev/dsp` remains available, and OSS/native opens share exclusive ownership.
There was no pre-existing native ALSA PCM endpoint in this checkout: the old
sound file implemented OSS, so the native endpoint is an additive ABI module.

Supported hardware is deliberately narrow: RW_INTERLEAVED, stereo S16_LE,
48000 Hz, four 1024-frame periods. HW_REFINE intersects masks and intervals;
unsupported formats/rates and empty/open-ended intersections are rejected.
No mmap, capture, pause, digital output or mixer controls are advertised.
ALSA's status/control mmap fallback uses SYNC_PTR; sample buffers use the
standard WRITEI_FRAMES ioctl, with checked usercopy and short transfer counts.
Pointers are period-granular (BATCH), software-staged frames stay before START
or the negotiated start threshold, and SW boundary wrap is reported without
accepting forged application DMA pointers. The fixed-size ring is never
resized to a userspace-provided buffer size. DRAIN flushes the last partial
period and waits DMA plus backend audible tail. DROP stops HDA before discarding
owned slots; VirtIO STOP/RELEASE must retire every used-ring buffer before reuse.
Uncertain retirement retains the owner instead of admitting a new opener.

The implementation is a bounded bring-up playback interface, not full ALSA
feature parity, low latency, or a guarantee against hardware underruns. The
backend's existing full-ring ambiguity checks still fail closed. HDMI is a
separate bounded DDI/HDA route below; analog PCM availability is not evidence
that the HDMI display route is available.

## Transport and graph

`tk-axdriver-hda` owns width-correct MMIO, CRST reset, 256-entry CORB/RIRB DMA,
AFG discovery and parameter/connection list queries. Serial verbs have bounded
250-ms response timeouts. Codec address, short/long connection lists and range
entries are decoded; disconnected/digital pins and graph cycles are rejected.
Preference is headphone, speaker, then line out. The selected path traverses
pins, selectors and mixers to a non-digital output converter. It powers the
AFG/widgets to D0, selects the connection, unmutes the selected input and
output amps using declared/inherited amp capabilities, sets pin OUT/HP and
EAPD where advertised, then programs converter stream tag/format. Vendor
coefficient verbs, GPIO quirks, jack switching and recording are not included.
Realtek boards may need those quirks after the first actual codec capture.

RIRB response-status generation is enabled while INTCTL stays zero and PCI
INTx remains masked. Completion is polled and acknowledged, not delivered to
an unowned vector. QEMU otherwise stalls its CORB at RINTCNT after one response.

The first output descriptor follows GCAP's input-stream count. Four 4096-byte
BDL slots carry coherent PCM DMA. Unique tokens retire from LPIB progression;
slots are zeroed after retirement. A new burst after draining resets the
stream before reusing its slots. A poll gap approaching a whole ring lap is
an error, not invented completion. This is a bounded polling bring-up driver,
not a low-latency or hardware-underrun-proof audio claim. Published allocations
are released only after stop/reset proves retirement; failure retains DMA.
If HDA is available it is the boot-selected backend for the existing sound
endpoint, otherwise VirtIO playback continues unchanged.

## Bounded DDI HDMI audio (software integrated; physical output unverified)

The D5 path is now wired through the same HDA playback owner. The original
display-side implementation is `crates/ax/tk-intel-display/src/audio.rs`; the
kernel adapter is `kernel/src/drm/intel/audio.rs`; HDA route discovery and
ELD publication live in `crates/ax/tk-axdriver-hda`. It is deliberately
limited to display 13 Pipe A, legacy HDMI on TC1/TC2, the enumerated HDMI audio
pixel-clock table, and two-channel LPCM S16_LE at 48 kHz. A sink must advertise
that format in a valid CTA audio block. Unsupported clocks, sink capabilities,
codec routes, and uncertain status are video-only or fail-closed states, not
guessed audio success.

The display handoff consumes the same validated source EDID used by the
connector. The original Rust CTA parser checks base/extension extents and
checksums, then builds the ELD baseline bytes; the baseline length is ELD byte
2. The HDA side validates the received ELD again and only admits the fixed
stereo/48-kHz/16-bit capability. The selected HSW/DDI fields program the HDMI
pixel-clock index and table N where available; the DDI M/CTS manual-enable
fields are cleared so hardware calculates CTS. A compiled C oracle uses the
unmodified Linux 7.2.3 MIT `intel_audio.c` clock/N/config/enable/disable
functions and selected `intel_audio_regs.h` definitions; a separate oracle
compares the unmodified MIT `drm_edid.c::drm_edid_to_eld` ELD assembly against
the Rust ELD bytes. The ELD oracle supplies controlled CTA data-block records
to DRM's builder, so it verifies baseline assembly, not the independent Rust
CTA parser.

The handoff is ordered after stable link and scanout proof and requires the
display audio power domain, Pipe A, DDI HDMI function, and DC-off state to be
already held/readable. It never requests or wakes a display power well. The
display presence/ELD sequence runs while TC power is retained; only then is
the validated ELD delivered to HDA. The ADL-P/N codec route maps PORT_D/TC1 to
pin NID 0x0a and PORT_E/TC2 to 0x0b, but those hints do not authorize writes by
themselves: live codec vendor, digital HDMI pin capabilities, connection,
widget graph, and converter PCM caps are checked. The fixed stereo converter
format and HDMI Audio InfoFrame are configured only on the confirmed route.
This is an original bounded Rust HDA behavior adaptation; no Linux GPL codec
implementation body is ported.

Before a modeset or TC/link/power release, an owned session invalidates ELD,
waits two fresh frames, disables output presence, and restores only its owned
fields; HDA then removes the ELD and retires playback DMA before TC teardown.
If HDA/DMA retirement or register readback is uncertain, the audio state is
quarantined and the link/power reference is retained. If no session is owned,
the gate reads the display output-enable and ELD-valid flags: clear flags plus
a valid power/link proof allow an inactive skip; active or unreadable status is
quarantined without writes or an assumption that HDA DMA stopped. A preexisting
`AlreadyEnabled` state is not adopted. Stable two-sample HPD disconnects run
the same retirement hook before reporting the physical connector state; an
audio error preserves the TC link/power quarantine, and reconnect only
re-publishes ELD after the stable video path is revalidated. A quarantined or
unsupported audio route can therefore leave video connected while audio stays
unavailable. There is no automatic takeover/recovery of foreign or uncertain
audio state.

Software evidence is limited to host models and source-oracle comparisons:
`tk-intel-display` covers landed-store rollback prefixes, vblank timeout,
inactive/active/unknown gating and clock-table decisions; HDA fake tests cover
port-specific route discovery, ELD switch, converter/InfoFrame setup, DMA
retirement and an unanswered mid-route verb invalidating the controller.
The explicit Linux source-oracle tests cover 14 clock/enable-disable pairs and
three exact ELD layouts. These tests do not exercise an N305 HDMI sink, the
physical PW_2/DC-off behavior, real HPD unplug/reconnect timing, HDMI HDA
converter behavior, or an audible display-speaker waveform. Existing QEMU HDA
WAV and native ALSA WAV checks exercise the HDA/ALSA playback backend and must
not be reported as HDMI audio acceptance. Physical HDMI output remains
**未在硬件上验证**.

GuC/HuC is not an analog HDA requirement. No SOF DSP firmware or generic
graphics register writes are performed by the HDA controller driver; only the
bounded kernel display adapter owns the display-side audio registers.

## Validation procedure

Host fake covers anonymous-codec graph selection, cycles, disconnected and
digital routes, wire layout, CORB/RIRB DMA, BDL memory, distinct tokens and
failure on an ambiguous full ring lap. `--audio-device hda --audio-backend wav`
appends `ich9-intel-hda` + `hda-duplex`, writes workdir/audio.wav and fixes the
backend to stereo S16 at 48000 Hz. VirtIO remains the default topology.

Compile `tests/guest/hda-smoke.c` statically and install into a copied rootfs.
It negotiates the existing OSS endpoint, writes 8192 stereo frames of a known
integer waveform, drains and closes. After guest forced poweroff/QEMU exit,
parse WAV and compare **all 32768 payload bytes in order** against the known
waveform. Leading/trailing device silence is permitted; missing, changed,
reordered or duplicated payload samples are not. The guest's submission
marker alone is never a pass. This validates QEMU's codec/stream transport,
not the physical headphone jack or a specific N305 codec.

Measured QEMU 10.2.2/KVM: `check_hda_waveform.py` passed all 8192 frames /
32768 bytes in the WAV, permitting surrounding silence only. An earlier
run lost the final backend-buffered samples even though DMA drained; release
now runs a silent tail for max(FIFOS + 1, two periods) before STOP. This is why
submission/initialization markers are not used as waveform acceptance.

Node IDs are bounded to the specification's seven bits; malformed child ranges
and long connection entries must not set the reserved indirect-address bit.
The regression test and final 8192-frame QEMU WAV comparison pass with that
validation (seven HDA host tests total).

## Native ALSA validation (continuation)

Build **unmodified** upstream alsa-lib 1.2.15.3 and alsa-utils 1.2.16 `aplay`
with a static musl toolchain (hardware PCM plugin enabled). Put `aplay`,
`tests/guest/alsa-hw.conf` and an 8192-frame WAV from
`tools/check_hda_waveform.py::expected_payload` into a disposable rootfs.
In the guest run:

```
ALSA_CONFIG_PATH=/alsa-hw.conf aplay -D hw:0,0 /input.wav
```

Run QEMU with `--audio-device hda --audio-backend wav`, power off after playback,
and run `tools/check_hda_waveform.py <workdir>/audio.wav`. The acceptance is all
8192 frames / 32768 bytes identical, with only surrounding silence permitted.
`tests/guest/alsa-smoke.c` additionally checks native control enumeration,
unsupported format rejection, prestart state/pointers, forged pointer rejection,
explicit START/DROP/PREPARE/HW_FREE, and exclusive OSS/native ownership.
The configuration file only selects ALSA's standard hardware plugin; it does
not modify the client, convert samples or route them through OSS.

Sources for the client: <https://www.alsa-project.org/wiki/Download> and
<https://www.alsa-project.org/files/pub/utils/>. ABI layouts were checked against
Linux `include/uapi/sound/asound.h`; the Rust byte codec and state machine are
original implementations.

Measured continuation: native `aplay -D hw:0,0` on QEMU HDA/KVM produced the
exact 8192-frame / 32768-byte waveform. The native ABI exerciser passed on HDA
and VirtIO. Four new ALSA host tests, eight HDA host tests, shared adapter
42 host tests and the full guest 52/52 gate passed. The additional VirtIO WAV
run used QEMU's existing 44100-Hz backend default (resampling), so its recording
is **not** claimed as a bit-identical 48000-Hz PCM test. Physical codec/headphone
output and HDMI remain unverified.
