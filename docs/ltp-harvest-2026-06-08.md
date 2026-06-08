# LTP Harvest 2026-06-08

This records the first bounded official unopened LTP harvest after the Docker
baseline in `docs/baseline-2026-06-08.md`.

Evidence rule: a case is promoted only when the lab parser recorded `pass` on
all four combinations:

- `rv/glibc`
- `rv/musl`
- `la/glibc`
- `la/musl`

`silent-pass`, `TCONF`-only, missing combinations, nonzero returns, timeouts,
and panics are not promoted.

## Runs

All runs used existing evaluator artifacts with `--skip-kernel-build` and
LTP-only support disks.

| Run | Case source | Result |
| --- | --- | --- |
| `.state/ltp-lab/runs/harvest-commands-0001` | `commands`, 5 selected cases | no promotions |
| `.state/ltp-lab/runs/harvest-syscalls-0001` | `syscalls`, 10 selected cases | no promotions; stopped by eventfd panic frontier |
| `.state/ltp-lab/runs/harvest-fsperms-0001` | `fs_perms_simple`, 3 unopened cases | no promotions; glibc pass, musl fail |
| `.state/ltp-lab/runs/harvest-dio-0001` | `dio`, offset 0, limit 2 | 2 promotions |
| `.state/ltp-lab/runs/harvest-dio-0002` | `dio`, offset 2, limit 4 | 4 promotions |
| `.state/ltp-lab/runs/harvest-dio-0003` | `dio`, offset 10, limit 5 | partial run stopped manually; `dio30` long-run/hang candidate |
| `.state/ltp-lab/runs/harvest-dio-0004` | `dio`, offset 10, limit 4 | 4 promotions |

## Promotions

These 10 cases passed all four combinations and were added to `ltp_test.txt`
near the existing DIO block:

| Case | Command | Evidence |
| --- | --- | --- |
| `dio16` | `diotest5 -b 65536 -i 1000` | `harvest-dio-0001`: rv pass=2, la pass=2 |
| `dio17` | `diotest6 -b 65536 -i 1000` | `harvest-dio-0001`: rv pass=2, la pass=2 |
| `dio18` | `diotest2 -b 65536 -i 1000 -o 1024000` | `harvest-dio-0002`: rv pass=2, la pass=2 |
| `dio19` | `diotest3 -b 65536 -i 1000 -o 1024000` | `harvest-dio-0002`: rv pass=2, la pass=2 |
| `dio20` | `diotest5 -b 65536 -i 1000 -o 1024000` | `harvest-dio-0002`: rv pass=2, la pass=2 |
| `dio21` | `diotest6 -b 65536 -i 1000 -o 1024000` | `harvest-dio-0002`: rv pass=2, la pass=2 |
| `dio26` | `diotest6 -b 8192 -v 100` | `harvest-dio-0004`: rv pass=2, la pass=2 |
| `dio27` | `diotest6 -b 8192 -o 1024000 -i 1000 -v 100` | `harvest-dio-0004`: rv pass=2, la pass=2 |
| `dio28` | `diotest6 -b 8192 -o 1024000 -i 1000 -v 200` | `harvest-dio-0004`: rv pass=2, la pass=2 |
| `dio29` | `diotest3 -b 65536 -n 100 -i 100 -o 1024000` | `harvest-dio-0004`: rv pass=2, la pass=2 |

## Attempted Failure Taxonomy

### Commands

Run: `harvest-commands-0001`

| Case | Matrix result | Taxonomy |
| --- | --- | --- |
| `ld01_sh` | RV nonzero, LA silent-pass | missing guest build tool (`gcc`) / TCONF-only on LA |
| `ldd01_sh` | all four fail | dynamic-loader command behavior mismatch |
| `nm01_sh` | RV nonzero, LA silent-pass | missing guest tool (`gcc`) / TCONF-only on LA |
| `gzip01_sh` | all four fail | command behavior or filesystem output mismatch |
| `df01_sh` | RV nonzero, LA pass | BusyBox `stat` option incompatibility on RV path |

### Syscalls

Run: `harvest-syscalls-0001`

The batch was intentionally treated as non-promotable after RV hit a panic
frontier. Missing combinations below mean the case did not run after the panic
or did not reach the second libc.

| Case | Matrix result | Taxonomy |
| --- | --- | --- |
| `getcpu01` | RV fail, LA fail | missing syscall: `getcpu()` returned `ENOSYS` |
| `getrusage03` | RV timeout, LA silent-pass | resource accounting / timeout semantics |
| `getrusage04` | RV nonzero, LA silent-pass | resource accounting or unsupported subcase |
| `futimesat01` | RV nonzero, LA silent-pass | timestamp syscall compatibility |
| `ftruncate04` | RV nonzero, LA silent-pass | truncate edge-case compatibility |
| `ftruncate04_64` | RV nonzero, LA silent-pass | truncate64 edge-case compatibility |
| `eventfd06` | RV panic, LA silent-pass | eventfd/synchronization panic frontier |

### File Permissions

Run: `harvest-fsperms-0001`

| Case | Matrix result | Taxonomy |
| --- | --- | --- |
| `fs_perms01` | rv/la glibc pass, rv/la musl fail | libc/rootfs-sensitive permission semantics |
| `fs_perms02` | rv/la glibc pass, rv/la musl fail | libc/rootfs-sensitive permission semantics |
| `fs_perms03` | rv/la glibc pass, rv/la musl fail | libc/rootfs-sensitive permission semantics |

### DIO

Runs: `harvest-dio-0001`, `harvest-dio-0002`, `harvest-dio-0003`,
`harvest-dio-0004`

- `dio16-21` and `dio26-29` are promoted.
- `dio30` was attempted in `harvest-dio-0003` and showed no log progress after
  `RUN LTP CASE dio30`; the run was stopped manually to keep the harvest
  bounded.
- `dio22-25` were intentionally not attempted in this harvest because they use
  the larger `-o 104857600` offset set and should be measured in a separate DIO
  batch.

## Remaining Frontier

After promotion and Docker inventory refresh at `2026-06-08T05:50:37`:

- current `ltp_test.txt`: 1099 entries
- all 1099 entries resolve on rv/la x glibc/musl
- remaining unopened available lines: 2724

Largest remaining runtest groups:

| Runtest | Remaining available lines |
| --- | ---: |
| `syscalls` | 398 |
| `controllers` | 335 |
| `ltp-aiodio.part1` | 140 |
| `net.nfs` | 113 |
| `scsi_debug.part1` | 112 |
| `net_stress.ipsec_udp` | 106 |
| `net_stress.ipsec_dccp` | 104 |
| `net_stress.ipsec_sctp` | 104 |
| `net_stress.ipsec_tcp` | 104 |
| `fs_bind` | 95 |
| `net_stress.ipsec_icmp` | 86 |
| `containers` | 83 |
| `ltp-aiodio.part2` | 83 |
| `mm` | 73 |
| `cve` | 69 |
| `net.features` | 62 |
| `ltp-aiodio.part4` | 59 |
| `fs_readonly` | 55 |
| `ltp-aio-stress` | 54 |
| `hugetlb` | 51 |
| `fs` | 50 |
| `net.sctp` | 41 |
| `net_stress.interface` | 25 |
| `net_stress.multicast` | 24 |
| `commands` | 23 |

Coarse taxonomy of the remaining frontier:

| Bucket | Remaining available lines |
| --- | ---: |
| network | 865 |
| namespace/cgroup/container | 441 |
| AIO/DIO/I/O stress | 369 |
| filesystem | 349 |
| general syscalls | 341 |
| other | 156 |
| memory | 141 |
| CVE/security regression | 51 |
| command/tooling | 11 |

## Next Harvest Batches

Priorities for the next bounded LTP harvest:

1. Re-run DIO without `dio30`: attempt `dio22-25` separately with a larger time
   budget, then retest `dio30` alone if needed.
2. Avoid mixing panic-prone syscall cases with unrelated candidates. Run
   `eventfd06` alone when debugging the panic.
3. Split `fs_perms_simple` work by libc because glibc already passes and musl
   fails on both arches.
4. Continue unopened batches with small, isolated groups and promotion only from
   all-four `pass` evidence.
