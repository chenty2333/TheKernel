# Vendored source record: `starry-smoltcp`

## Immutable published baseline

- Registry package: `starry-smoltcp` `0.12.1-preview.1`
- crates.io archive SHA-256:
  `77517d20c697d9cc6fc579fa3e199c53e64c875d7e2898e17c41918e19e84e4b`
- Repository declared by the package:
  <https://github.com/Starry-OS/smoltcp.git>
- Cargo VCS context commit:
  `7401a54b041924a78971b077cd62140b26d441dc`
- Cargo VCS dirty flag: `true`. The commit identifies repository context, not
  the exact published tree; only the archive checksum is an exact baseline.
- Authors and copyright: retained from smoltcp contributors.
- License: `0BSD`; `LICENSE-0BSD.txt` is retained.
- Original manifest: `Cargo.toml.orig`
- Cargo source record: `.cargo_vcs_info.json`

The checksum above was verified against the downloaded crates.io archive. The
fork itself derives from smoltcp; its README, changelog, authors, and license
must remain attributed to the smoltcp contributors.

## TheKernel patch lineage

- `2aee66603219c493fad33d0e8f8734b53c5eb925` introduced the local published
  snapshot.
- `667169b70580f8afef54416253e368108eb1d0a2` corrected TCP receive-window and
  timestamp handling.
- `96df7d9b5a2bb86e83d2f92b9d9a31b279407b03` added fallible buffer-replacement
  hooks used by the kernel network layer.

Against the verified archive, the current `src/` patch is 143 insertions and
5 deletions across TCP, UDP, and async-waker code. It includes:

- TCP and UDP receive/transmit buffer replacement with state/emptiness checks;
- receive-window validation and a focused TCP regression test;
- reset of SACK, duplicate-ACK, timestamp, and window-tracking state;
- reset-time async-waker clearing for TheKernel's wrapper model.

Whitespace-only changes also exist in the changelog, one example, the snapshot
fixture, and the final license newline.

## Known boundary

The current async-waker `clear` implementation uses `mem::forget` on a
registered `Waker`. That avoids invoking a potentially stale wake vtable but
can leak a waker reference. This provenance record does not endorse it as a
stable ownership contract; future lifecycle work must replace it with a safe,
bounded registration/cancellation design.

When rebasing, compare against the archive checksum rather than the dirty VCS
context commit, retain 0BSD attribution, and independently revalidate every
network-state modification under loss, close/reset, and non-loopback traffic.
