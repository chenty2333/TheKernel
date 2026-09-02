#!/usr/bin/env python3
"""Self-contained unit tests for the Linux v6.18 static routing gate."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/ci/linux_v618_abi_gate.py"
SPEC = importlib.util.spec_from_file_location("linux_v618_abi_gate", SCRIPT)
assert SPEC and SPEC.loader
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


class GateTests(unittest.TestCase):
    def manifest(self) -> Path:
        return ROOT / "config/linux-v6.18-abi.toml"

    def source(self, root: Path) -> tuple[Path, dict[int, str]]:
        manifest = gate.load_manifest(self.manifest())
        values = set()
        for key in ("ordinary_explicit", "explicit_enosys", "native_fallback"):
            values |= gate.numbers(manifest["routing_inventory"][key], key)
        entries = {number: f"sys_{number}" for number in values}
        entries.update({134: "uselib", 156: "_sysctl", 174: "create_module", 177: "get_kernel_syms", 178: "query_module", 180: "nfsservctl", 181: "getpmsg", 182: "putpmsg", 183: "afs_syscall", 184: "tuxcall", 185: "security", 205: "set_thread_area", 211: "get_thread_area", 212: "lookup_dcookie", 214: "epoll_ctl_old", 215: "epoll_wait_old", 236: "vserver", 321: "bpf", 335: "uretprobe", 336: "uprobe", 463: "setxattrat", 464: "getxattrat", 465: "listxattrat", 466: "removexattrat", 467: "open_tree_attr", 468: "file_getattr", 469: "file_setattr"})
        table = root / manifest["linux"]["table"]
        table.parent.mkdir(parents=True)
        table.write_text("\n".join(f"{number} common {name}" for number, name in sorted(entries.items())) + "\n512 x32 ignored\n", encoding="utf-8")
        return root, entries

    def dispatch(self, path: Path, entries: dict[int, str], fallback: bool = False, expression: str = "sys_call()") -> None:
        manifest = gate.load_manifest(self.manifest())
        inventory = manifest["routing_inventory"]
        ordinary = gate.numbers(inventory["ordinary_explicit"], "ordinary_explicit")
        if fallback: ordinary |= gate.numbers(inventory["native_fallback"], "native_fallback")
        ni = gate.numbers(inventory["explicit_enosys"], "explicit_enosys")
        arms = [f"Sysno::{entries[number]} => {expression}," for number in sorted(ordinary)]
        arms.append(" | ".join(f"Sysno::{entries[number]}" for number in sorted(ni)) + " => sys_ni_syscall(),")
        arms.append("_ => Err(AxError::Unsupported),")
        path.write_text("fn dispatch_syscall(sysno: Sysno) { match sysno {\n" + "\n".join(arms) + "\n} }", encoding="utf-8")

    def contract_dispatch(self, path: Path, entries: dict[int, str]) -> None:
        self.dispatch(path, entries)
        routes = {
            321: '#[cfg(feature = "bpf")] Sysno::bpf => super::bpf::sys_bpf(memory, 0, 0, 0),',
            335: "Sysno::uretprobe => super::task::sys_uretprobe(uctx),",
            336: "Sysno::uprobe => super::task::sys_uprobe(uctx),",
            463: "Sysno::setxattrat => super::fs::sys_setxattrat(memory),",
            464: "Sysno::getxattrat => super::fs::sys_getxattrat(memory),",
            465: "Sysno::listxattrat => super::fs::sys_listxattrat(memory),",
            466: "Sysno::removexattrat => super::fs::sys_removexattrat(memory),",
            467: "Sysno::open_tree_attr => super::fs::sys_open_tree_attr(memory),",
            468: "Sysno::file_getattr => super::fs::sys_file_getattr(memory),",
            469: "Sysno::file_setattr => super::fs::sys_file_setattr(memory),",
        }
        text = path.read_text(encoding="utf-8")
        for number, route in routes.items():
            text = text.replace(f"Sysno::{entries[number]} => sys_call(),", route)
        path.write_text(text, encoding="utf-8")

    def contracts(self, root: Path, change: tuple[str, str] | None = None) -> Path:
        text = (ROOT / "config/linux-v6.18-contracts.toml").read_text(encoding="utf-8")
        if change is not None:
            text = text.replace(*change)
        path = root / "contracts.toml"; path.write_text(text, encoding="utf-8")
        return path

    def oracles(self, root: Path, change: tuple[str, str] | None = None) -> Path:
        text = (ROOT / "config/linux-v6.18-oracles.toml").read_text(encoding="utf-8")
        if change is not None:
            text = text.replace(*change)
        path = root / "oracles.toml"; path.write_text(text, encoding="utf-8")
        return path

    def test_pin_and_terminal_are_fixed(self) -> None:
        manifest = gate.load_manifest(self.manifest())
        self.assertEqual(manifest["linux"]["tag"], "v6.18")
        self.assertEqual(manifest["terminal"], gate.TERMINAL)

    def test_inventory_accepts_canonical_decimal_strings_only(self) -> None:
        self.assertEqual(gate.numbers(["179", "180-181"], "ordinary_explicit"), {179, 180, 181})
        for value in (
            "0179",
            "+179",
            " 179",
            "179 ",
            "-1",
            "1_79",
            "0179-180",
            "179-0180",
            "179-",
            "179-180-181",
        ):
            with self.subTest(value=value):
                with self.assertRaisesRegex(gate.GateError, "invalid number/range"):
                    gate.numbers([value], "ordinary_explicit")

    def test_x32_is_excluded(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            source, _ = self.source(Path(temporary) / "linux")
            self.assertEqual(len(gate.parse_table(source / gate.load_manifest(self.manifest())["linux"]["table"])), 383)

    def test_inventory_passes_and_final_names_nine_missing_routes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            source, entries = self.source(Path(temporary) / "linux")
            dispatch = Path(temporary) / "dispatch.rs"; self.dispatch(dispatch, entries)
            gate.inventory(self.manifest(), source, dispatch)
            routes = dispatch.read_text(encoding="utf-8")
            for number in (335, 336, 463, 464, 465, 466, 467, 468, 469):
                routes = routes.replace(f"Sysno::{entries[number]} => sys_call(),\n", "")
            dispatch.write_text(routes, encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "terminal explicit routes mismatch") as raised:
                gate.final(self.manifest(), source, dispatch)
        self.assertEqual(sum(entries[number] in str(raised.exception) for number in (335, 336, 463, 464, 465, 466, 467, 468, 469)), 9)

    def test_literals_and_comments_cannot_invent_routes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text('/* fn dispatch_syscall(x) { match sysno { Sysno::fake => {} } } */\nconst X: &str = r#"Sysno::fake => {}"#; const Y: u8 = b\'}\';\nfn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::real => { let _ = "},"; ok() }, Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }', encoding="utf-8")
            found, ni, _ = gate.routes(path, {"real", "ni"}, gate.WITNESS)
        self.assertEqual(found, {"real", "ni"}); self.assertEqual(ni, {"ni"})

    def test_pattern_comment_cannot_invent_route(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text("fn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::real /* Sysno::fake */ => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }", encoding="utf-8")
            found, _, _ = gate.routes(path, {"real", "fake", "ni"}, gate.WITNESS)
        self.assertEqual(found, {"real", "ni"})

    def test_macro_dispatch_decoy_is_not_selected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text("macro_rules! decoy { () => { fn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::fake => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } } }; }\nfn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::real => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }", encoding="utf-8")
            found, ni, _ = gate.routes(path, {"real", "ni"}, gate.WITNESS)
        self.assertEqual(found, {"real", "ni"})
        self.assertEqual(ni, {"ni"})

    def test_guard_and_false_cfg_are_rejected(self) -> None:
        for arm, message in (("Sysno::real if false => ok(),", "match guard"), ("#[cfg(any())] Sysno::real => ok(),", "conditional attribute")):
            with self.subTest(arm=arm), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "dispatch.rs"
                path.write_text(f"fn dispatch_syscall(sysno: Sysno) {{ match sysno {{ {arm} Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), }} }}", encoding="utf-8")
                with self.assertRaisesRegex(gate.GateError, message): gate.routes(path, {"real", "ni"}, gate.WITNESS)

    def test_exact_bpf_witness_and_placeholders(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text('#[allow(dead_code)] fn dispatch_syscall(sysno: Sysno) { match sysno { #[cfg(feature = "bpf")] Sysno::bpf => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }', encoding="utf-8")
            found, _, _ = gate.routes(path, {"bpf", "ni"}, gate.WITNESS)
            self.assertEqual(found, {"bpf", "ni"})
        with tempfile.TemporaryDirectory() as temporary:
            source, entries = self.source(Path(temporary) / "linux")
            path = Path(temporary) / "dispatch.rs"; self.dispatch(path, entries, True, "wrap(sys_ni_syscall())")
            with self.assertRaisesRegex(gate.GateError, "reach sys_ni_syscall"): gate.final(self.manifest(), source, path)

    def test_contract_cells_are_individual_and_honest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            cells = gate.contract_cells(self.contracts(root), entries)
            self.assertEqual(set(cells), {134, 156, 174, 177, 178, 180, 181, 182, 183, 184, 185, 205, 211, 212, 214, 215, 236, 321, 335, 336, 463, 464, 465, 466, 467, 468, 469})
            self.assertTrue(all(cell["handler"].endswith(":sys_ni_syscall") for cell in cells.values() if cell["status"] == "explicit-enosys"))
            self.assertEqual(cells[321]["conditional"], "bpf")

    def test_all_declared_non_ni_cells_bind_dispatch_handlers(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            dispatch = root / "dispatch.rs"; self.contract_dispatch(dispatch, entries)
            gate.contract_cells(self.contracts(root), entries, dispatch)
            text = dispatch.read_text(encoding="utf-8").replace("super::bpf::sys_bpf(memory, 0, 0, 0)", "other_real_handler(memory)")
            dispatch.write_text(text, encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "non-NI cell is not bound"):
                gate.contract_cells(self.contracts(root), entries, dispatch)
            self.contract_dispatch(dispatch, entries)
            text = dispatch.read_text(encoding="utf-8").replace("super::fs::sys_setxattrat(memory)", "other_module::sys_setxattrat(memory)")
            dispatch.write_text(text, encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "non-NI cell is not bound"):
                gate.contract_cells(self.contracts(root), entries, dispatch)

    def test_static_schema_accepts_contracts_without_oracle_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            dispatch = root / "dispatch.rs"; self.contract_dispatch(dispatch, entries)
            gate.schema(self.manifest(), self.contracts(root), self.oracles(root), source, dispatch)

    def test_graph_fields_reject_empty_or_handler_defined_payloads(self) -> None:
        for value in (["flag:"], ["errno:handler-defined"]):
            with self.subTest(value=value):
                with self.assertRaisesRegex(gate.GateError, "typed grammar|placeholder"):
                    gate.graph_field(value, "v618-test", "flags" if value[0].startswith("flag:") else "errno_order")

    def test_handler_cfg_must_match_cell_conditional(self) -> None:
        with tempfile.TemporaryDirectory(dir=ROOT) as temporary:
            path = Path(temporary) / "handler.rs"
            path.write_text('#[cfg(feature = "other")]\nfn handler() {}\n', encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "cfg does not match"):
                gate.rust_function(path, "handler", "bpf")

    def test_handler_must_be_a_top_level_rust_item(self) -> None:
        with tempfile.TemporaryDirectory(dir=ROOT) as temporary:
            path = Path(temporary) / "handler.rs"
            path.write_text("macro_rules! decoy { () => { fn macro_handler() {} }; }\nfn outer() {\n    fn nested_handler() {}\n}\n", encoding="utf-8")
            for symbol in ("macro_handler", "nested_handler"):
                with self.subTest(symbol=symbol):
                    with self.assertRaisesRegex(gate.GateError, "top-level match"):
                        gate.rust_function(path, symbol, "explicit-none")

    def test_contract_rejects_placeholder_unbound_handler_and_duplicate_cell(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); _, entries = self.source(root / "linux")
            for change, message in (
                (('errno_order = ["errno:ENOSYS"]', 'errno_order = ["Linux syscall-specific order"]'), "placeholder"),
                (("handler = \"kernel/src/syscall/dispatch.rs:sys_ni_syscall\"", "handler = \"kernel/src/syscall/dispatch.rs:not_a_function\""), "function definition"),
                (("number = 156\nname = \"_sysctl\"", "number = 134\nname = \"uselib\""), "duplicate"),
            ):
                with self.subTest(change=change):
                    with self.assertRaisesRegex(gate.GateError, message):
                        gate.contract_cells(self.contracts(root, change), entries)

    def test_oracle_rejects_missing_source_and_vacuous_witness_config(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaisesRegex(gate.GateError, "shared guest binary source does not exist"):
                gate.validate_oracles(self.oracles(root), {})


if __name__ == "__main__": unittest.main()
