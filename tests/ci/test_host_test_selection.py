"""The host suite's component membership must be declared, never defaulted."""
import json
import subprocess
import unittest

from tests.support import repo_root
from tools import thekernel as product
from tools.product_state import ProductError


def package(name, layer, selected=None):
    settings = {"layer": layer}
    if selected is not None:
        settings["host-test"] = {"selected": selected}
    return {"name": name, "metadata": {"thekernel": settings}}


def names(packages):
    return sorted(item["name"] for item in product.host_test_selection(packages))


class HostTestSelectionTests(unittest.TestCase):
    def test_mechanism_and_linux_abi_are_members_without_declaring_it(self):
        self.assertEqual(
            names([package("queue", "mechanism"), package("abi", "linux_abi")]),
            ["abi", "queue"])

    def test_declared_platform_component_joins_the_suite(self):
        self.assertEqual(names([package("task", "platform", True)]), ["task"])

    def test_platform_component_declared_out_stays_out(self):
        self.assertEqual(names([package("task", "platform", False)]), [])

    def test_undeclared_platform_component_fails_instead_of_vanishing(self):
        with self.assertRaises(ProductError) as raised:
            product.host_test_selection([package("task", "platform")])
        self.assertIn("task", str(raised.exception))

    def test_a_non_boolean_declaration_is_not_a_declaration(self):
        with self.assertRaises(ProductError):
            product.host_test_selection([package("task", "platform", "true")])

    def test_every_undeclared_component_is_named_at_once(self):
        with self.assertRaises(ProductError) as raised:
            product.host_test_selection(
                [package("task", "platform"), package("driver", "platform")])
        message = str(raised.exception)
        self.assertIn("driver", message)
        self.assertIn("task", message)

    def test_the_repository_itself_declares_every_platform_component(self):
        metadata = subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
            cwd=repo_root(), capture_output=True, text=True, check=True)
        product.host_test_selection(json.loads(metadata.stdout)["packages"])


if __name__ == "__main__":
    unittest.main()
