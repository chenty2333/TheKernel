"""Exercise metadata policy independently of filesystem layout."""
import unittest
from pathlib import Path
from tests.support import load_script_module, test_tmpdir

gate = load_script_module("cargo_dependency_layers", "scripts/ci/check_cargo_dependency_layers.py")


def package(name, layer, dependencies=()):
    return {
        "id": name, "name": name, "manifest_path": f"/workspace/{name}/Cargo.toml",
        "metadata": {"thekernel": {"layer": layer}},
        "targets": [{"name": name, "kind": ["lib"]}],
        "dependencies": [{"name": dep, "path": f"/workspace/{dep}"} for dep in dependencies],
    }


class CargoDependencyLayersTests(unittest.TestCase):
    def check(self, packages, members=None):
        return gate.violations({"packages": packages, "workspace_members": members or [p["id"] for p in packages]}, Path("/workspace"))

    def test_platform_can_consume_mechanism(self):
        self.assertEqual(self.check([package("driver", "platform", ["queue"]), package("queue", "mechanism")]), [])

    def test_mechanism_cannot_consume_runtime(self):
        self.assertEqual(self.check([package("queue", "mechanism", ["runtime"]), package("runtime", "platform")]), ["queue (mechanism) depends on runtime (platform)"])

    def test_linux_cannot_consume_platform_even_when_in_same_directory(self):
        self.assertEqual(self.check([package("abi", "linux_abi", ["task"]), package("task", "platform")]), ["abi (linux_abi) depends on task (platform)"])

    def test_integration_accepts_all_lower_layers(self):
        self.assertEqual(self.check([package("kernel", "integration", ["abi", "task", "queue"]), package("abi", "linux_abi"), package("task", "platform"), package("queue", "mechanism")]), [])

    def test_metadata_required(self):
        self.assertEqual(self.check([package("queue", None)]), ["queue: missing or invalid package.metadata.thekernel.layer"])

    def test_uncontrolled_optional_dependency_rejected(self):
        p = package("queue", "mechanism", ["outside"])
        p["dependencies"][0]["optional"] = True
        self.assertEqual(self.check([p]), ["queue: uncontrolled path dependency outside"])

    def test_external_shadow_rejected(self):
        external = package("queue", "mechanism")
        external["id"] = "registry-queue"
        self.assertEqual(self.check([package("queue", "mechanism"), external], ["queue"]), ["external dependency duplicates controlled workspace package queue"])


class WorkspacePolicyTests(unittest.TestCase):
    def setUp(self):
        temporary = test_tmpdir()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        (self.root / "Cargo.toml").write_text(
            '[workspace.package]\nrust-version = "1.100"\npublish = false\nrepository = "repo"\n')
        self.component = self.root / "crates" / "tk-queue"
        self.component.mkdir(parents=True)
        self.package = package("tk-queue", "mechanism")
        self.package.update(manifest_path=str(self.component / "Cargo.toml"),
                            rust_version="1.100", publish=[], repository="repo")

    def check(self):
        return gate.workspace_policy_violations(
            {"packages": [self.package], "workspace_members": [self.package["id"]]}, self.root)

    def test_shared_policy_accepted(self):
        self.assertEqual(self.check(), [])

    def test_compiler_release_and_repository_drift_rejected(self):
        self.package.update(rust_version="1.85", publish=None, repository="old")
        self.assertEqual(len(self.check()), 3)

    def test_legacy_package_name_rejected(self):
        self.package["name"] = "thekernel-queue"
        self.assertIn("tk- prefix", self.check()[0])

    def test_nested_toolchain_override_rejected(self):
        (self.component.parent / "rust-toolchain.toml").write_text('[toolchain]\nchannel="stable"\n')
        self.assertIn("overrides workspace compiler", self.check()[0])


if __name__ == "__main__":
    unittest.main()
