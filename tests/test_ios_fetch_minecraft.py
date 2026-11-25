"""The iOS prepare workflow's download of the version JSON (packaging/ios/prepare/fetch_minecraft.py)."""
import contextlib
import hashlib
import importlib.util
import io
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "fetch_minecraft", ROOT / "packaging/ios/prepare/fetch_minecraft.py"
)
fetch_minecraft = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetch_minecraft)


def sha1(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


class VersionJson(unittest.TestCase):
    """Mojang serves each version JSON it has published at its own checksum's address."""

    def setUp(self):
        self.pinned = b'{"id": "26.3", "republished": false}'
        self.republished = b'{"id": "26.3", "republished": true}'
        self.lock = {
            "version": "26.3",
            "manifest_sha1": sha1(self.pinned),
            "accepted_manifest_sha1": [],
        }
        self.served = {}
        for data in (self.pinned, self.republished):
            url = f"https://piston-meta.mojang.com/v1/packages/{sha1(data)}/26.3.json"
            self.served[url] = data

    def listing(self, data: bytes) -> dict:
        """Mojang's version manifest, listing `data` as 26.3's JSON."""
        url = f"https://piston-meta.mojang.com/v1/packages/{sha1(data)}/26.3.json"
        return {"versions": [{"id": "26.3", "url": url, "sha1": sha1(data)}]}

    def version_json(self, listing: dict) -> bytes:
        with contextlib.redirect_stdout(io.StringIO()):
            return fetch_minecraft.version_json(self.lock, listing, self.served.__getitem__)

    def test_a_listed_json_research_lock_knows_is_used(self):
        self.assertEqual(self.version_json(self.listing(self.pinned)), self.pinned)
        self.lock["accepted_manifest_sha1"] = [sha1(self.republished)]
        self.assertEqual(self.version_json(self.listing(self.republished)), self.republished)

    def test_a_republished_json_falls_back_to_the_pinned_one(self):
        self.assertEqual(self.version_json(self.listing(self.republished)), self.pinned)

    def test_a_pinned_json_that_does_not_match_its_checksum_stops_the_run(self):
        pinned_url = f"https://piston-meta.mojang.com/v1/packages/{sha1(self.pinned)}/26.3.json"
        self.served[pinned_url] = b"something else"
        with self.assertRaises(SystemExit):
            self.version_json(self.listing(self.republished))


if __name__ == "__main__":
    unittest.main()
