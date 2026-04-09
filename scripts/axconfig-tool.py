#!/usr/bin/env python3

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path


def load_toml(path: Path) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def deep_merge(base: dict, overlay: dict) -> dict:
    for key, value in overlay.items():
        if isinstance(value, dict) and isinstance(base.get(key), dict):
            deep_merge(base[key], value)
        else:
            base[key] = value
    return base


def get_nested(config: dict, dotted_key: str):
    cur = config
    for part in dotted_key.split("."):
        if not isinstance(cur, dict) or part not in cur:
            raise KeyError(dotted_key)
        cur = cur[part]
    return cur


def set_nested(config: dict, dotted_key: str, value) -> None:
    parts = dotted_key.split(".")
    cur = config
    for part in parts[:-1]:
        nxt = cur.get(part)
        if not isinstance(nxt, dict):
            nxt = {}
            cur[part] = nxt
        cur = nxt
    cur[parts[-1]] = value


def parse_override(spec: str):
    key, sep, raw_value = spec.partition("=")
    if not sep:
        raise ValueError(f"invalid override {spec!r}")
    try:
        value = tomllib.loads(f"value = {raw_value}")["value"]
    except tomllib.TOMLDecodeError as err:
        raise ValueError(f"invalid override {spec!r}: {err}") from err
    return key, value


def render_value(value) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(render_value(item) for item in value) + "]"
    raise TypeError(f"unsupported value type: {type(value)!r}")


def build_template(configs: list[str]) -> str:
    sections = []
    for config in configs:
        text = Path(config).read_text(encoding="utf-8").rstrip()
        if text:
            sections.append(text)
    return "\n\n".join(sections).rstrip() + "\n"


def replace_assignments(text: str, overrides: list[tuple[str, object]]) -> str:
    pending: dict[tuple[str, ...], dict[str, object]] = {}
    for dotted_key, value in overrides:
        parts = dotted_key.split(".")
        pending.setdefault(tuple(parts[:-1]), {})[parts[-1]] = value

    header_re = re.compile(r"^\s*\[([A-Za-z0-9_.-]+)\]\s*$")
    assign_re = re.compile(r"^(\s*)([A-Za-z0-9_-]+)(\s*=\s*)(.*?)(\s*(#.*)?)$")
    current_table: tuple[str, ...] = ()
    out_lines = []

    for line in text.splitlines():
        header_match = header_re.match(line)
        if header_match:
            current_table = tuple(
                part for part in header_match.group(1).split(".") if part
            )
            out_lines.append(line)
            continue

        assign_match = assign_re.match(line)
        table_overrides = pending.get(current_table)
        if assign_match and table_overrides:
            key = assign_match.group(2)
            if key in table_overrides:
                replacement = render_value(table_overrides.pop(key))
                line = (
                    f"{assign_match.group(1)}{key}{assign_match.group(3)}"
                    f"{replacement}{assign_match.group(5)}"
                )
        out_lines.append(line)

    missing = [
        ".".join((*table, key))
        for table, values in pending.items()
        for key in values
    ]
    if missing:
        raise KeyError(", ".join(sorted(missing)))
    return "\n".join(out_lines).rstrip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Subset-compatible repo-local replacement for axconfig-gen."
    )
    parser.add_argument("configs", nargs="*", help="input TOML config files")
    parser.add_argument("-o", "--output", help="output TOML path")
    parser.add_argument("-r", "--read", help="read a dotted key from the merged config")
    parser.add_argument(
        "-w",
        "--write",
        action="append",
        default=[],
        help="override dotted key with a TOML value, e.g. plat.max-cpu-num=1",
    )
    parser.add_argument(
        "-c",
        "--current",
        help="overlay an existing config before applying -w overrides",
    )
    args = parser.parse_args()

    try:
        if args.read:
            merged: dict = {}
            for config_path in args.configs:
                deep_merge(merged, load_toml(Path(config_path)))
            if args.current:
                deep_merge(merged, load_toml(Path(args.current)))
            print(render_value(get_nested(merged, args.read)))
            return 0

        if not args.output:
            parser.error("-o/--output is required unless -r/--read is used")

        text = build_template(args.configs)
        if args.write:
            text = replace_assignments(
                text, [parse_override(spec) for spec in args.write]
            )
        Path(args.output).write_text(text, encoding="utf-8")
        return 0
    except FileNotFoundError as err:
        print(err, file=sys.stderr)
        return 1
    except (KeyError, TypeError, ValueError, tomllib.TOMLDecodeError) as err:
        print(err, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
