# Official Evaluator Snapshot

This directory contains the official OSComp judge scripts imported as a local
snapshot. The local evaluator uses these scripts for judge semantics only. It
does not use the official Docker-oriented QEMU, prework, postwork, `/mnt/cghook`,
or `pygrading.Job` controller path.

Imported source:

- repository: `https://github.com/oscomp/autotest-for-oskernel.git`
- commit: `d1bb3a3c4b27274e196a2648518525c1a304e339`
- source path used for this import: `/home/ava/Desktop/autotest-for-oskernel`

The inspected source checkout did not contain a top-level `LICENSE`, `COPYING`,
or `NOTICE` file. Keep the exact source URL, commit, and imported file list in
`manifest.json` so future refreshes are auditable.

Refresh from an explicit local checkout only:

```bash
./scripts/oscomp.sh official-refresh \
  --source /home/ava/Desktop/autotest-for-oskernel
```

The command copies only `kernel/judge/judge_*.py` and
`kernel/judge/config.json`, then rewrites `manifest.json` with the source repo,
commit, source status, imported file list, and added/removed/changed file
summary. It does not fetch from the network and does not import the official
QEMU, prework, postwork, template, or `pygrading.Job` controller path.

After refreshing, run:

```bash
python3 -m unittest discover -s tests/oscomp_eval -v
python3 -m compileall -q tools/oscomp_eval scripts/validate-oscomp-output.py scripts/ltp-lab.py tests/oscomp_eval
```
