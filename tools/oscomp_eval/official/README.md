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
or `NOTICE` file. The exact source URL, commit, and imported file list are
recorded in `manifest.json`.

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

Validation commands:

```bash
PYTHONPATH=. python3 -m unittest discover -s tests/oscomp_eval -v
python3 -m py_compile \
  tools/oscomp_eval/*.py \
  tools/oscomp_eval/lab/*.py \
  tools/oscomp_eval/lab/plugins/**/*.py \
  scripts/ltp-lab.py \
  tests/oscomp_eval/*.py
```
