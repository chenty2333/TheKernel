"""Focused-lab plugin registry."""

from __future__ import annotations

from .base import GroupPlugin
from .groups import GenericGroupPlugin, LtpGroupPlugin


GENERIC_GROUPS = (
    "basic",
    "busybox",
    "libctest",
    "lua",
    "iperf",
    "cyclictest",
    "netperf",
    "libcbench",
    "iozone",
    "lmbench",
)


def group_plugins() -> dict[str, GroupPlugin]:
    plugins: dict[str, GroupPlugin] = {"ltp": LtpGroupPlugin()}
    for group in GENERIC_GROUPS:
        plugins[group] = GenericGroupPlugin(group)
    return plugins


def plugin_for(group: str) -> GroupPlugin:
    plugins = group_plugins()
    try:
        return plugins[group]
    except KeyError as error:
        raise ValueError(f"unsupported lab group: {group}") from error

