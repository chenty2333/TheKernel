# Intel HDA analog playback (2026-10-04)

## Facts, boundaries and references

N305 is `8086:54c8`, class 0403 / interface 00; subsystem `10ec:12ec` is
not a codec model identification. The driver reads codec vendor IDs and walks
the widget graph, rather than guessing a Realtek model. **未在硬件上验证**.

Original Rust; consulted Intel HDA 1.0a §§3–7 (PDF/text under external refs/audio),
Linux 7.2.3 `sound/hda/{controllers/intel.c,core/controller.c,common/codec.c,
codecs/generic.c}`. Those are the relocated paths of the older `sound/pci/hda`
files named in the request. QEMU's CORB/RIRB response-count behavior was checked
against <https://github.com/qemu/qemu/blob/master/hw/audio/intel-hda.c>.

**The current kernel audio interface is OSS `/dev/dsp`, not ALSA PCM.** This
change reuses that endpoint and its existing bounded playback worker. Native
`/dev/snd/pcmC0D0p`, ALSA control/PCM ioctls, mmap status/control and a direct
`aplay -D hw:0,0` path are not implemented, so B2 is only partially complete.
An ALSA OSS plugin may use `/dev/dsp`; this is not claimed as direct ALSA
support. The driver currently offers stereo S16LE at 48000 Hz only.

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

## HDMI dependency

Alder Lake HDMI codec availability and ELD depend on display power domains,
DDI/transcoder configuration and a live HDMI/DP link. The firmware-framebuffer
path does not own that sequencing, so HDMI/digital widgets are intentionally
excluded. Future work must coordinate a display-power reference with HDA,
read ELD after link/modeset success, configure converter/channel slots, and
release/reset audio before the display link powers down. GuC/HuC is not an
analog HDA requirement. No SOF DSP firmware or graphics register writes are
performed by this driver.

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
