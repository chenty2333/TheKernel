"""Command line entrypoint for the local OSComp evaluator."""

from __future__ import annotations

from .cli import main


if __name__ == "__main__":
    raise SystemExit(main())
