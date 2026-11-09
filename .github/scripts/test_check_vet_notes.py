"""Tests for check-vet-notes.py. Run: python3 -B -m unittest discover -s .github/scripts"""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("check_vet_notes", Path(__file__).with_name("check-vet-notes.py"))
check_vet_notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_vet_notes)

CONFIG = "supply-chain/config.toml"
AUDITS = "supply-chain/audits.toml"
GOOD = "Added by Satheesh Kumar because png's test-only tree is out of scope here. Review in #297."
OLD = '[[exemptions.ahash]]\nversion = "0.8.12"\ncriteria = "safe-to-deploy"\n'


def exemption(crate, version, criteria="safe-to-run", notes=None):
    text = f'\n[[exemptions.{crate}]]\nversion = "{version}"\ncriteria = "{criteria}"\n'
    return text + (f'notes = "{notes}"\n' if notes is not None else "")


def trusted(crate, user, notes=None):
    text = f'\n[[trusted.{crate}]]\ncriteria = "safe-to-deploy"\nuser-id = {user}\nstart = "2019-01-01"\nend = "2027-01-01"\n'
    return text + (f'notes = "{notes}"\n' if notes is not None else "")


class CheckVetNotes(unittest.TestCase):
    def test_existing_entries_without_notes_pass(self):
        self.assertEqual(check_vet_notes.check({CONFIG: OLD}, {CONFIG: OLD}), [])

    def test_this_repository_passes_against_itself(self):
        root = Path(__file__).parents[2]
        texts = {p: (root / p).read_text(encoding="utf-8") for p in (CONFIG, AUDITS)}
        self.assertEqual(check_vet_notes.check(texts, texts), [])

    def test_new_entries_with_full_notes_pass(self):
        head = {CONFIG: OLD + exemption("png", "0.18.0", notes=GOOD), AUDITS: trusted("serde", 3618, GOOD)}
        self.assertEqual(check_vet_notes.check({CONFIG: OLD}, head), [])

    def test_new_exemption_without_notes_fails(self):
        errors = check_vet_notes.check({CONFIG: OLD}, {CONFIG: OLD + exemption("png", "0.18.0")})
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("[[exemptions.png]] 0.18.0", errors[0])
        self.assertIn("has no notes", errors[0])

    def test_new_trusted_entry_without_an_issue_fails(self):
        notes = "mozilla and bytecode-alliance trust this publisher for this crate, added by Satheesh."
        errors = check_vet_notes.check({}, {AUDITS: trusted("serde", 3618, notes)})
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("#N", errors[0])

    def test_notes_must_name_who_and_why(self):
        errors = check_vet_notes.check({}, {CONFIG: exemption("png", "0.18.0", notes="Review in #297.")})
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("who", errors[0])
        self.assertIn("why", errors[0])

    def test_a_new_version_or_higher_criteria_counts_as_added(self):
        head = {CONFIG: exemption("ahash", "0.8.13", "safe-to-deploy") + exemption("ahash", "0.8.12", "safe-to-run")}
        self.assertEqual(len(check_vet_notes.check({CONFIG: OLD}, head)), 2)

    def test_renewing_a_trusted_entry_is_not_an_addition(self):
        base = trusted("serde", 3618, "old notes")
        self.assertEqual(check_vet_notes.check({AUDITS: base}, {AUDITS: base.replace("2027", "2028")}), [])


if __name__ == "__main__":
    unittest.main()
