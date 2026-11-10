"""固定源码补查、Cargo.lock 归档校验与引用授权回归。"""

import hashlib
import io
import json
import pathlib
import subprocess
import tarfile
import tempfile
import unittest

import review_recheck as review
from review_sources import Sources


class SourceFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.repo, self.cargo = self.root / "repo", self.root / "cargo"
        self.repo.mkdir()
        self.git("init", "-qb", "main")
        self.archives, self.checksums = {}, {}
        for version in ("1.0.0", "2.0.0"):
            content = f"// pinned {version}\nfn spawn_on() {{}}\n// closed accepts new tasks\n"
            self.make_archive(version, {"src/lib.rs": content.encode(), "blob.bin": b"\0binary"})
        self.revisions = []
        for number, content in enumerate(("old caller\nold guard\n", "new caller\nnew guard\n", "LATER_FIX_SECRET\n")):
            (self.repo / "main.rs").write_text(content)
            (self.repo / "a.rs").write_text("needle\n")
            (self.repo / "z.rs").write_text("needle " + "x" * 60 + "\n" + "line\n" * 205)
            versions = ("1.0.0",) if number == 0 else ("1.0.0", "2.0.0")
            (self.repo / "Cargo.lock").write_text("version = 4\n" + "".join(
                f'[[package]]\nname="widget"\nversion="{version}"\n'
                f'source="registry+https://example.invalid/index"\nchecksum="{self.checksums[version]}"\n'
                for version in versions))
            self.commit()
            self.revisions.append(self.git("rev-parse", "HEAD").strip())
        self.packet = review.make_packet({"name": "fixture", "split": "unscored", "target": self.revisions[1],
            "candidates": [{"id": "C1", "question": "Does the caller preserve the guard?", "expected": "unknown",
                "evidence": [{"version": "after", "path": "main.rs", "start": 1, "end": 2}]},
                {"id": "C2", "question": "Does the library reject spawn after close?", "expected": "unknown",
                "evidence": [{"version": "before", "path": "main.rs", "start": 1, "end": 2}]}]}, self.repo)

    def git(self, *args):
        return review.git(self.repo, *args)

    def commit(self):
        self.git("add", "-A")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "Fixture")

    def make_archive(self, version, files):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            for path, raw in files.items():
                info = tarfile.TarInfo(f"widget-{version}/{path}")
                info.size = len(raw)
                archive.addfile(info, io.BytesIO(raw))
            info = tarfile.TarInfo(f"widget-{version}/host-link")
            info.type, info.linkname = tarfile.SYMTYPE, "/outside/secret"
            archive.addfile(info)
        path = self.cargo / "registry/cache/fixture" / f"widget-{version}.crate"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(buffer.getvalue())
        self.archives[version], self.checksums[version] = path, hashlib.sha256(buffer.getvalue()).hexdigest()

    def sources(self, **kwargs):
        return Sources(self.repo, self.packet, cargo_home=self.cargo, **kwargs)

    def read(self, sources, **updates):
        args = {"candidate_id": "C1", "version": "after", "package": "", "path": "main.rs", "start": 1, "end": 2}
        return sources.call("source_read", {**args, **updates})


class SourceTests(SourceFixture):
    def test_git_reads_ignore_current_checkout_and_later_fix(self):
        (self.repo / "main.rs").write_text("UNCOMMITTED_SECRET\n")
        sources = self.sources()
        after, before = self.read(sources), self.read(sources, version="before")
        self.assertEqual(after["sources"][0]["text"], "1: new caller\n2: new guard")
        self.assertEqual(before["sources"][0]["text"], "1: old caller\n2: old guard")
        self.assertNotIn("SECRET", json.dumps(sources.validation_packet(self.packet)))

    def test_dependency_names_are_discovered_from_each_frozen_lockfile(self):
        sources = self.sources()
        result = sources.call("source_packages", {"candidate_id": "C2", "version": "after", "query": "widget"})
        self.assertEqual([p["package"] for p in result["packages"]], ["widget@1.0.0", "widget@2.0.0"])
        self.assertEqual(self.read(sources, package="widget")["status"], "unavailable")
        self.assertEqual(self.read(sources, version="before", package="widget@2.0.0")["status"], "unavailable")

    def test_dependency_reads_verified_archive_and_not_extracted_host_copy(self):
        poison = self.cargo / "registry/src/fixture/widget-2.0.0/src/lib.rs"
        poison.parent.mkdir(parents=True)
        poison.write_text("HOST_POISON")
        sources = self.sources()
        result = self.read(sources, package="widget@2.0.0", path="src/lib.rs", end=3)
        self.assertEqual(result["status"], "ok")
        evidence = result["sources"][0]
        self.assertIn("pinned 2.0.0", evidence["text"])
        self.assertNotIn("HOST_POISON", evidence["text"])
        self.assertEqual(evidence["archive_sha256"], self.checksums["2.0.0"])

    def test_missing_or_tampered_archive_is_unavailable_and_grants_no_source(self):
        for action in (lambda p: p.write_bytes(b"tampered"), lambda p: p.unlink()):
            path = self.archives["2.0.0"]
            saved = path.read_bytes()
            action(path)
            sources = self.sources()
            result = self.read(sources, package="widget@2.0.0", path="src/lib.rs")
            self.assertEqual(result["status"], "unavailable")
            self.assertEqual(sources.evidence, [])
            path.write_bytes(saved)

    def test_paths_never_escape_source_or_follow_archive_links(self):
        sources = self.sources()
        for path in ("../secret", "/outside/secret", ".git/config"):
            with self.subTest(path=path):
                self.assertEqual(self.read(sources, path=path)["status"], "unavailable")
        self.assertEqual(self.read(sources, package="widget@2.0.0", path="host-link")["status"], "unavailable")

    def test_git_and_dependency_literal_search_return_real_numbered_sources(self):
        sources = self.sources()
        for package, query, expected in (("", "caller", "1: new caller"), ("widget@2.0.0", "spawn_on", "2: fn spawn_on() {}")):
            result = sources.call("source_search", {"candidate_id": "C1", "version": "after",
                                                    "package": package, "path": "", "query": query})
            self.assertEqual(result["status"], "ok")
            self.assertEqual(len(result["matches"]), 1)
            self.assertIn(expected, result["sources"][0]["text"])

    def test_failed_tool_call_does_not_authorize_partially_collected_evidence(self):
        sources = self.sources(max_chars=25)
        result = sources.call("source_search", {"candidate_id": "C1", "version": "after",
                                                "package": "", "path": "", "query": "needle"})
        self.assertEqual(result["status"], "unavailable")
        self.assertEqual(sources.evidence, [])
        self.assertEqual(sources.grants, {})
        self.assertIn("200 lines", self.read(sources, path="z.rs", end=201)["missing"])

    def test_source_citations_are_granted_only_to_the_requesting_candidate(self):
        sources = self.sources()
        result = self.read(sources)
        packet = sources.validation_packet(self.packet)
        row = {"id": "C1", "verdict": "excluded", "severity": None, "reason": "Source disproves it",
               "evidence": [{"source": result["sources"][0]["id"], "line": 1}], "missing": ""}
        other = {**row, "id": "C2"}
        with self.assertRaisesRegex(ValueError, "outside"):
            review.parse_verdicts(json.dumps({"verdicts": [row, other]}), packet)
        other["evidence"] = [{"source": "E2", "line": 1}]
        self.assertEqual(review.parse_verdicts(json.dumps({"verdicts": [row, other]}), packet), [row, other])

    def test_unrecognized_candidate_and_binary_source_do_not_produce_evidence(self):
        sources = self.sources()
        self.assertEqual(self.read(sources, candidate_id="unknown")["status"], "unavailable")
        self.assertEqual(self.read(sources, package="widget@2.0.0", path="blob.bin")["status"], "unavailable")

    def test_fake_pins_cannot_traverse_cargo_cache(self):
        (self.repo / "Cargo.lock").write_text('version=4\n[[package]]\nname="../escape"\nversion="2.0.0"\n'
            'source="registry+https://example.invalid/index"\nchecksum="' + '0' * 64 + '"\n')
        self.commit()
        target = self.git("rev-parse", "HEAD").strip()
        packet = {**self.packet, "target": target, "baseline": self.revisions[-1]}
        sources = Sources(self.repo, packet, cargo_home=self.cargo)
        result = self.read(sources, package="../escape@2.0.0", path="src/lib.rs")
        self.assertEqual(result["status"], "unavailable")
        self.assertIn("invalid package", result["missing"])


if __name__ == "__main__":
    unittest.main()
