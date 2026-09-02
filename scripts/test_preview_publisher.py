from __future__ import annotations

import io
import json
import re
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

from unittest import mock

import scripts.smarty_preview_trusted as trusted
import scripts.test_preview_promotion as promotion_tests


STRICT_YAML_TO_JSON = r'''
require "json"
require "psych"

def convert(node)
  case node
  when Psych::Nodes::Mapping
    abort "tagged or anchored mapping" unless node.anchor.nil? && node.tag.nil?
    result = {}
    node.children.each_slice(2) do |key, value|
      abort "non-scalar mapping key" unless key.is_a?(Psych::Nodes::Scalar) && key.anchor.nil? && key.tag.nil?
      abort "duplicate mapping key: #{key.value}" if result.key?(key.value)
      result[key.value] = convert(value)
    end
    result
  when Psych::Nodes::Sequence
    abort "tagged or anchored sequence" unless node.anchor.nil? && node.tag.nil?
    node.children.map { |child| convert(child) }
  when Psych::Nodes::Scalar
    abort "tagged or anchored scalar" unless node.anchor.nil? && node.tag.nil?
    node.value
  else
    abort "unsupported YAML node"
  end
end

stream = Psych.parse_stream(STDIN.read)
abort "expected one YAML document" unless stream.children.one?
puts JSON.generate(convert(stream.children.first.root))
'''


def load_workflow(path: Path) -> dict[str, object]:
    result = subprocess.run(
        ["ruby", "-e", STRICT_YAML_TO_JSON],
        input=path.read_text(encoding="utf-8"),
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        raise AssertionError(result.stderr.strip())
    return json.loads(result.stdout)


class TrustedCliTests(unittest.TestCase):
    def test_cli_parser_builds(self) -> None:
        self.assertIn("validate-sources", trusted.build_parser().format_help())
        self.assertIn("validate-existing-release", trusted.build_parser().format_help())
        self.assertIn("validate-existing-release-metadata", trusted.build_parser().format_help())
        self.assertIn("validate-pair-attestation", trusted.build_parser().format_help())


class TrustedPairAttestationTests(unittest.TestCase):
    commit = "1" * 40

    def _report(self, subject: Path) -> list[dict[str, object]]:
        record = trusted.file_record(subject)
        invocation = f"https://github.com/{trusted.REPOSITORY}/actions/runs/42/attempts/1"
        return [{
            "verificationResult": {
                "verifiedIdentity": {"runnerEnvironment": "github-hosted"},
                "signature": {"certificate": {
                    "subjectAlternativeName": trusted.PUBLISH_CERT_IDENTITY,
                    "issuer": "https://token.actions.githubusercontent.com",
                    "githubWorkflowTrigger": "workflow_run",
                    "githubWorkflowSHA": self.commit,
                    "githubWorkflowName": trusted.PUBLISH_WORKFLOW_NAME,
                    "githubWorkflowRepository": trusted.REPOSITORY,
                    "githubWorkflowRef": trusted.PUBLISH_SOURCE_REF,
                    "buildSignerURI": trusted.PUBLISH_CERT_IDENTITY,
                    "buildSignerDigest": self.commit,
                    "runnerEnvironment": "github-hosted",
                    "sourceRepositoryURI": f"https://github.com/{trusted.REPOSITORY}",
                    "sourceRepositoryDigest": self.commit,
                    "sourceRepositoryRef": trusted.PUBLISH_SOURCE_REF,
                    "buildConfigURI": trusted.PUBLISH_CERT_IDENTITY,
                    "buildConfigDigest": self.commit,
                    "buildTrigger": "workflow_run",
                    "runInvocationURI": invocation,
                }},
                "statement": {
                    "_type": "https://in-toto.io/Statement/v1",
                    "subject": [{"name": subject.name, "digest": {"sha256": record["sha256"]}}],
                    "predicateType": "https://slsa.dev/provenance/v1",
                    "predicate": {
                        "buildDefinition": {
                            "buildType": "https://actions.github.io/buildtypes/workflow/v1",
                            "externalParameters": {"workflow": {
                                "path": trusted.PUBLISH_WORKFLOW_PATH,
                                "ref": trusted.PUBLISH_SOURCE_REF,
                                "repository": f"https://github.com/{trusted.REPOSITORY}",
                            }},
                            "internalParameters": {"github": {"runner_environment": "github-hosted"}},
                            "resolvedDependencies": [{
                                "uri": trusted.PUBLISH_SOURCE_URI,
                                "digest": {"gitCommit": self.commit},
                            }],
                        },
                        "runDetails": {
                            "builder": {"id": trusted.PUBLISH_CERT_IDENTITY},
                            "metadata": {
                                "invocationId": invocation,
                            },
                        },
                    },
                },
            },
        }]

    def test_validator_extracts_attested_publisher_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            subject = Path(directory) / "smarty-pair.json"
            subject.write_bytes(b"attested pair")
            result = trusted.validate_pair_attestation(self._report(subject), subject)
            self.assertEqual(result["publisher_commit"], self.commit)
            self.assertEqual(result["subject"], {"name": subject.name, **trusted.file_record(subject)})

    def test_validator_rejects_subject_or_publisher_identity_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            subject = Path(directory) / "smarty-pair.json"
            subject.write_bytes(b"attested pair")
            report = self._report(subject)
            report[0]["verificationResult"]["statement"]["subject"][0]["digest"]["sha256"] = "f" * 64
            with self.assertRaisesRegex(ValueError, "subject mismatch"):
                trusted.validate_pair_attestation(report, subject)
            report = self._report(subject)
            dependency = report[0]["verificationResult"]["statement"]["predicate"]["buildDefinition"]["resolvedDependencies"][0]
            dependency["uri"] = "git+https://github.com/example/repo@refs/heads/master"
            with self.assertRaisesRegex(ValueError, "source URI mismatch"):
                trusted.validate_pair_attestation(report, subject)
            report = self._report(subject)
            report[0]["verificationResult"]["statement"]["predicate"]["buildDefinition"]["resolvedDependencies"][0]["digest"]["gitCommit"] = "2" * 40
            with self.assertRaisesRegex(ValueError, "does not match its certificate"):
                trusted.validate_pair_attestation(report, subject)
            report = self._report(subject)
            report[0]["verificationResult"]["signature"]["certificate"]["subjectAlternativeName"] += "-evil"
            with self.assertRaisesRegex(ValueError, "certificate subjectAlternativeName mismatch"):
                trusted.validate_pair_attestation(report, subject)


class TrustedArtifactDownloadTests(unittest.TestCase):
    def test_cross_origin_redirect_strips_github_authorization(self) -> None:
        request = trusted.Request(
            "https://api.github.com/repos/Smarty-Pants-Inc/herdr/actions/artifacts/42/zip",
            headers={"Authorization": "Bearer secret", "Accept": "application/vnd.github+json"},
        )
        redirected = trusted._HttpsArtifactRedirectHandler().redirect_request(
            request,
            None,
            302,
            "Found",
            {},
            "https://productionresultssa.blob.core.windows.net/actions-results/signed",
        )
        self.assertIsNotNone(redirected)
        self.assertIsNone(redirected.get_header("Authorization"))

    def test_artifact_redirect_rejects_non_https_targets(self) -> None:
        request = trusted.Request(
            "https://api.github.com/repos/Smarty-Pants-Inc/herdr/actions/artifacts/42/zip",
            headers={"Authorization": "Bearer secret"},
        )
        with self.assertRaisesRegex(ValueError, "HTTPS URL"):
            trusted._HttpsArtifactRedirectHandler().redirect_request(
                request, None, 302, "Found", {}, "http://example.com/artifact.zip"
            )

    def test_download_rejects_foreign_initial_url_before_opening(self) -> None:
        identity = {
            "artifacts": {
                "candidate-macos-x86_64-1": {
                    "id": 42,
                    "size_in_bytes": 1,
                    "archive_download_url": "https://example.com/artifact.zip",
                }
            }
        }
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(trusted, "build_opener") as opener:
            with self.assertRaisesRegex(ValueError, "not canonical"):
                trusted.download_artifacts(Path(directory), identity, "secret")
        opener.assert_not_called()

    def test_producer_artifacts_reconstruct_download_action_layout(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = root / "archives"
            output = root / "producer"
            archives.mkdir()
            fixtures = {
                "smarty-release-plan-2": {"producer-plan.json": b"plan"},
                "smarty-candidate-sources-2": {
                    "herdr-source.tar": b"source",
                    "omp-descriptor.json": b"descriptor",
                },
                "smarty-candidate-handoff-2": {
                    "candidate-artifacts/herdr-linux-x86_64": b"handoff",
                    "producer-handoff.json": b"handoff-record",
                },
                "candidate-linux-x86_64-2": {"herdr-linux-x86_64": b"raw"},
            }
            for name, members in fixtures.items():
                with zipfile.ZipFile(archives / f"{name}.zip", "w") as archive:
                    for member, payload in members.items():
                        archive.writestr(member, payload)
            identity = {"artifacts": {name: {} for name in fixtures}}
            trusted.extract_producer_artifacts(archives, output, identity)
            self.assertEqual((output / "producer-plan.json").read_bytes(), b"plan")
            self.assertEqual((output / "source-archives/herdr-source.tar").read_bytes(), b"source")
            self.assertEqual((output / "source-archives/omp-descriptor.json").read_bytes(), b"descriptor")
            self.assertEqual((output / "candidate-artifacts/herdr-linux-x86_64").read_bytes(), b"handoff")
            self.assertFalse((output / "herdr-linux-x86_64").exists())


class TrustedWorkflowRegistrationTests(unittest.TestCase):
    def test_producer_workflow_is_registered_on_protected_default(self) -> None:
        workflows = Path(__file__).resolve().parents[1] / ".github/workflows"
        producer = load_workflow(workflows / "smarty-preview.yml")
        publisher = load_workflow(workflows / "smarty-preview-publish.yml")
        self.assertEqual(producer["name"], "Smarty Preview")
        self.assertEqual(producer["on"], {"push": {"tags": ["smarty-preview-*"]}})
        self.assertEqual(
            publisher["on"],
            {"workflow_run": {"workflows": ["Smarty Preview"], "types": ["completed"]}},
        )

class TrustedSourceTests(unittest.TestCase):
    parent = "1" * 40
    source = "2" * 40
    omp = "3" * 40
    parent_tree = "4" * 40
    omp_tree = "5" * 40
    version = "17.4.0"
    build_id = f"managed-omp-{omp_tree}"

    def test_validate_sources_binds_parent_release_and_checkout_trees(self) -> None:
        tag = f"smarty-preview-2026-08-22-p{self.parent}-r{self.source}-o{self.omp}"
        identity = {
            "tag": tag,
            "build_id": tag.removeprefix("smarty-preview-"),
            "parent": self.parent,
            "source": self.source,
            "omp": self.omp,
        }
        plan = dict(identity)
        descriptor = {
            "repository": trusted.OMP_REPOSITORY,
            "commit": self.omp,
            "tree": self.omp_tree,
            "version": self.version,
            "build_id": self.build_id,
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent_root = root / "parent"
            omp_root = root / "omp"
            release_path = parent_root / "integrations/omp/release.json"
            release_path.parent.mkdir(parents=True)
            release_path.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "sourceRevision": self.omp,
                        "sourceTree": self.omp_tree,
                        "version": self.version,
                        "buildId": self.build_id,
                    }
                ),
                encoding="utf-8",
            )
            package_path = omp_root / "packages/coding-agent/package.json"
            package_path.parent.mkdir(parents=True)
            package_path.write_text(json.dumps({"version": self.version}), encoding="utf-8")

            def git(command: list[str], *, text: bool) -> str:
                self.assertTrue(text)
                checkout = Path(command[2])
                operation = command[3:]
                if operation == ["rev-parse", "HEAD"]:
                    return self.parent if checkout == parent_root else self.omp
                if operation == ["rev-parse", "HEAD^{tree}"]:
                    return self.parent_tree if checkout == parent_root else self.omp_tree
                if operation == ["ls-tree", "HEAD", "repos/herdr"]:
                    return f"160000 commit {self.source}\trepos/herdr"
                if operation == ["ls-tree", "HEAD", "repos/omp"]:
                    return f"160000 commit {self.omp}\trepos/omp"
                raise AssertionError(command)

            output = root / "trusted-source-record.json"
            with mock.patch.object(trusted.subprocess, "check_output", side_effect=git):
                result = trusted.validate_trusted_sources(
                    identity, plan, parent_root, omp_root, descriptor, output
                )
            self.assertEqual(result["omp"]["build_id"], self.build_id)
            self.assertEqual(json.loads(output.read_text(encoding="utf-8")), result)
            self.assertEqual(trusted.managed_omp_build_id(self.omp_tree), self.build_id)
            bad_descriptor = {**descriptor, "build_id": f"managed-omp-{self.omp}"}
            with mock.patch.object(trusted.subprocess, "check_output", side_effect=git):
                with self.assertRaises(ValueError):
                    trusted.validate_trusted_sources(
                        identity, plan, parent_root, omp_root, bad_descriptor, output
                    )

            release = json.loads(release_path.read_text(encoding="utf-8"))
            release["buildId"] = "wrong-build"
            release_path.write_text(json.dumps(release), encoding="utf-8")
            with mock.patch.object(trusted.subprocess, "check_output", side_effect=git):
                with self.assertRaises(ValueError):
                    trusted.validate_trusted_sources(
                        identity, plan, parent_root, omp_root, descriptor, output
                    )


class TrustedIdentityTests(unittest.TestCase):
    parent = "1" * 40
    source = "2" * 40
    omp = "3" * 40
    tag = f"smarty-preview-2026-08-22-p{parent}-r{source}-o{omp}"

    def test_paired_identity_binds_source_and_timestamp_day(self) -> None:
        identity = trusted.paired_identity(
            self.tag,
            source_sha=self.source,
            built_at="2026-08-22T03:00:00-07:00",
        )
        self.assertEqual(identity["build_id"], self.tag.removeprefix("smarty-preview-"))
        self.assertEqual(identity["day"], "2026-08-22")

    def test_paired_identity_rejects_wrong_source_or_day(self) -> None:
        with self.assertRaises(ValueError):
            trusted.paired_identity(self.tag, source_sha="4" * 40)
        with self.assertRaises(ValueError):
            trusted.paired_identity(self.tag, source_sha=self.source, built_at="2026-08-23T00:00:00Z")


class TrustedRunTests(unittest.TestCase):
    parent = "1" * 40
    source = "2" * 40
    omp = "3" * 40
    tag = f"smarty-preview-2026-08-22-p{parent}-r{source}-o{omp}"

    def _records(self, attempt: int = 2) -> list[dict[str, object]]:
        values = []
        for index, kind in enumerate(trusted.PRODUCER_ARTIFACT_KINDS, start=1):
            values.append(
                {
                    "id": 1000 + index,
                    "name": f"{kind}-{attempt}",
                    "expired": False,
                    "size_in_bytes": 1,
                    "digest": "sha256:" + "a" * 64,
                    "archive_download_url": f"https://api.github.com/repos/{trusted.REPOSITORY}/actions/artifacts/{1000 + index}/zip",
                    "created_at": "2026-08-22T00:00:01Z",
                    "updated_at": "2026-08-22T00:00:02Z",
                    "workflow_run": {"id": 42, "head_sha": self.source, "run_attempt": attempt},
                }
            )
        return values

    def _run(self, attempt: int = 2) -> dict[str, object]:
        return {
            "id": 42,
            "repository": {"full_name": trusted.REPOSITORY},
            "event": "push",
            "status": "completed",
            "conclusion": "success",
            "name": trusted.WORKFLOW_NAME,
            "path": trusted.WORKFLOW_PATH,
            "workflow_id": 77,
            "run_attempt": attempt,
            "run_started_at": "2026-08-21T23:59:59Z",
            "head_repository": {"full_name": trusted.REPOSITORY},
            "head_sha": self.source,
            "head_commit": {"id": self.source, "timestamp": "2026-08-22T03:00:00Z"},
            "head_branch": self.tag,
        }

    def test_run_validation_requires_exact_attempt_scoped_artifacts(self) -> None:
        result = trusted.validate_workflow_run(
            self._run(),
            {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
            {"total_count": 8, "artifacts": self._records()},
            run_id=42,
        )
        self.assertEqual(result["run_attempt"], 2)
        self.assertEqual(set(result["artifacts"]), set(trusted._artifact_names(2)))

    def test_run_validation_rejects_wrong_artifact_attempt(self) -> None:
        records = self._records()
        records[-1]["name"] = "smarty-candidate-handoff-1"
        with self.assertRaises(ValueError):
            trusted.validate_workflow_run(
                self._run(),
                {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
                {"total_count": 8, "artifacts": records},
                run_id=42,
            )

    def test_run_validation_binds_workflow_event_attempt(self) -> None:
        result = trusted.validate_workflow_run(
            self._run(),
            {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
            {"total_count": 8, "artifacts": self._records()},
            run_id=42,
            event_run_attempt=2,
        )
        self.assertEqual(result["event_run_attempt"], 2)
        with self.assertRaises(ValueError):
            trusted.validate_workflow_run(
                self._run(),
                {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
                {"total_count": 8, "artifacts": self._records()},
                run_id=42,
                event_run_attempt=1,
            )
    def test_run_validation_allows_stale_prior_attempt_artifacts(self) -> None:
        stale = self._records(attempt=1)
        current = self._records(attempt=2)
        result = trusted.validate_workflow_run(
            self._run(attempt=2),
            {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
            {"total_count": 16, "artifacts": stale + current},
            run_id=42,
        )
        self.assertEqual(set(result["artifacts"]), set(trusted._artifact_names(2)))

    def test_run_validation_rejects_future_attempt_artifacts(self) -> None:
        with self.assertRaisesRegex(ValueError, "future run attempt"):
            trusted.validate_workflow_run(
                self._run(attempt=2),
                {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
                {"total_count": 8, "artifacts": self._records(attempt=3)},
                run_id=42,
            )
    def test_run_validation_rejects_preseeded_current_attempt_artifact(self) -> None:
        records = self._records()
        records[0]["created_at"] = "2026-08-21T23:59:58Z"
        with self.assertRaisesRegex(ValueError, "predates"):
            trusted.validate_workflow_run(
                self._run(),
                {"id": 77, "name": trusted.WORKFLOW_NAME, "path": trusted.WORKFLOW_PATH},
                {"total_count": 8, "artifacts": records},
                run_id=42,
            )


class TrustedArchiveTests(unittest.TestCase):
    def test_tar_traversal_and_links_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            traversal = root / "traversal.tar"
            with tarfile.open(traversal, "w") as archive:
                info = tarfile.TarInfo("../escape")
                info.size = 1
                import io
                archive.addfile(info, io.BytesIO(b"x"))
            with self.assertRaises(ValueError):
                trusted._safe_tar(traversal, "fixture")

            linked = root / "linked.tar"
            with tarfile.open(linked, "w") as archive:
                info = tarfile.TarInfo("link")
                info.type = tarfile.SYMTYPE
                info.linkname = "target"
                archive.addfile(info)
            with self.assertRaises(ValueError):
                trusted._safe_tar(linked, "fixture")

    def test_producer_and_native_git_archive_modes_are_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repository = root / "repository"
            repository.mkdir()
            subprocess.run(["git", "init"], cwd=repository, check=True, capture_output=True)
            (repository / "regular").write_text("regular\n", encoding="utf-8")
            executable = repository / "executable"
            executable.write_text("#!/bin/sh\n", encoding="utf-8")
            executable.chmod(0o755)
            (repository / ".gitattributes").write_text("*.cmd text eol=crlf\n", encoding="utf-8")
            (repository / "windows.cmd").write_text("line\n", encoding="utf-8")
            subprocess.run(["git", "add", "."], cwd=repository, check=True)
            link_blob = subprocess.check_output(
                ["git", "hash-object", "-w", "--stdin"],
                cwd=repository,
                input=b"regular",
            ).decode("ascii").strip()
            subprocess.run(
                ["git", "update-index", "--add", "--cacheinfo", f"120000,{link_blob},link"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-m",
                    "fixture",
                ],
                cwd=repository,
                check=True,
                capture_output=True,
            )
            archive_path = root / "source.tar"
            subprocess.run(
                ["git", "archive", "--format=tar", "--output", str(archive_path), "HEAD"],
                cwd=repository,
                check=True,
            )
            with tarfile.open(archive_path) as archive:
                self.assertEqual(archive.getmember("regular").mode, 0o664)
                self.assertEqual(archive.getmember("executable").mode, 0o775)
                windows = archive.extractfile("windows.cmd")
                self.assertIsNotNone(windows)
                assert windows is not None
                self.assertEqual(windows.read(), b"line\r\n")
                link = archive.getmember("link")
                self.assertTrue(link.issym())
                self.assertEqual(link.mode, 0o777)
                self.assertEqual(link.linkname, "regular")
            (repository / ".gitattributes").write_text("*.cmd text eol=lf\n", encoding="utf-8")
            trusted.validate_git_archive(archive_path, repository)
            producer_archive = root / "producer.tar"
            with tarfile.open(producer_archive, "w") as archive:
                for name, payload, mode in (
                    (".gitattributes", b"*.cmd text eol=crlf\n", 0o644),
                    ("regular", b"regular\n", 0o644),
                    ("executable", b"#!/bin/sh\n", 0o755),
                    ("windows.cmd", b"line\n", 0o644),
                ):
                    info = tarfile.TarInfo(name)
                    info.mode = mode
                    info.size = len(payload)
                    archive.addfile(info, io.BytesIO(payload))
                link = tarfile.TarInfo("link")
                link.type = tarfile.SYMTYPE
                link.linkname = "regular"
                link.mode = 0
                archive.addfile(link)
            trusted.validate_git_archive(producer_archive, repository)

    def test_tar_extraction_writes_only_regular_members(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive_path = root / "source.tar"
            with tarfile.open(archive_path, "w") as archive:
                payload = b"ok"
                info = tarfile.TarInfo("src/file")
                info.size = len(payload)
                info.mode = 0o755
                archive.addfile(info, __import__("io").BytesIO(payload))
            output = root / "out"
            trusted.extract_tar(archive_path, output)
            self.assertEqual((output / "src/file").read_bytes(), b"ok")
            self.assertEqual((output / "src/file").stat().st_mode & 0o777, 0o755)

    def test_tar_extraction_preserves_safe_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive_path = root / "symlink.tar"
            with tarfile.open(archive_path, "w") as archive:
                payload = b"ok"
                info = tarfile.TarInfo("src/file")
                info.size = len(payload)
                archive.addfile(info, io.BytesIO(payload))
                link = tarfile.TarInfo("CLAUDE.md")
                link.type = tarfile.SYMTYPE
                link.linkname = "src/file"
                archive.addfile(link)
            output = root / "out"
            trusted.extract_tar(archive_path, output)
            self.assertTrue((output / "CLAUDE.md").is_symlink())
            self.assertEqual((output / "CLAUDE.md").read_bytes(), b"ok")

    def test_tar_aggregate_expanded_limit_is_enforced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive_path = root / "aggregate.tar"
            with tarfile.open(archive_path, "w") as archive:
                payload = b"12"
                info = tarfile.TarInfo("src/file")
                info.size = len(payload)
                archive.addfile(info, io.BytesIO(payload))
            old_total = trusted.MAX_TOTAL_EXTRACTED_BYTES
            try:
                trusted.MAX_TOTAL_EXTRACTED_BYTES = 1
                with self.assertRaises(ValueError):
                    trusted.extract_tar(archive_path, root / "out")
            finally:
                trusted.MAX_TOTAL_EXTRACTED_BYTES = old_total
    def test_zip_rejects_root_and_drive_members(self) -> None:
        for name in ("/escape", "C:/escape", "C:escape"):
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    trusted._safe_member_name(name, "fixture.zip")

    def test_zip_member_and_expanded_limits_are_enforced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive_path = root / "fixture.zip"
            with zipfile.ZipFile(archive_path, "w") as archive:
                archive.writestr("one", b"1")
                archive.writestr("two", b"2")
            output = root / "out"
            old_members = trusted.MAX_ZIP_MEMBERS
            old_bytes = trusted.MAX_ZIP_EXPANDED_BYTES
            old_total = trusted.MAX_TOTAL_EXTRACTED_BYTES
            try:
                trusted.MAX_ZIP_MEMBERS = 1
                with self.assertRaises(ValueError):
                    trusted.extract_zip(archive_path, output)
                trusted.MAX_ZIP_MEMBERS = 200_000
                trusted.MAX_ZIP_EXPANDED_BYTES = 1
                with self.assertRaises(ValueError):
                    trusted.extract_zip(archive_path, output)
                trusted.MAX_ZIP_EXPANDED_BYTES = 4 * 1024 * 1024 * 1024
                trusted.MAX_TOTAL_EXTRACTED_BYTES = 1
                with self.assertRaises(ValueError):
                    trusted.extract_zip(archive_path, root / "aggregate-out")
            finally:
                trusted.MAX_ZIP_MEMBERS = old_members
                trusted.MAX_ZIP_EXPANDED_BYTES = old_bytes
                trusted.MAX_TOTAL_EXTRACTED_BYTES = old_total


class TrustedSealTests(unittest.TestCase):
    parent = "1" * 40
    source = "2" * 40
    omp = "3" * 40

    def _fixture(self, root: Path) -> tuple[Path, dict[str, str]]:
        tag = f"smarty-preview-2026-08-22-p{self.parent}-r{self.source}-o{self.omp}"
        identity = {
            "tag": tag,
            "build_id": tag.removeprefix("smarty-preview-"),
            "built_at": "2026-08-22T00:00:00Z",
            "parent": self.parent,
            "source": self.source,
            "omp": self.omp,
        }
        assets = root / "assets"
        assets.mkdir()
        for name in trusted.PAYLOAD_ASSETS:
            (assets / name).write_bytes(b"payload")
            (assets / f"{name}.sha256").write_text(
                f"{trusted.file_record(assets / name)['sha256']}  {name}\n", encoding="ascii"
            )
        pair = {
            "release": {
                "repository": trusted.REPOSITORY,
                "tag": tag,
                "build_id": identity["build_id"],
                "built_at": identity["built_at"],
                "immutable": True,
            },
            "sources": {
                "parent": {"commit": self.parent},
                "herdr": {"commit": self.source},
                "omp": {"commit": self.omp},
            },
        }
        (assets / "smarty-pair.json").write_text(json.dumps(pair), encoding="utf-8")
        for name in trusted.FULL_RELEASE_ASSETS:
            path = assets / name
            if not path.exists():
                path.write_bytes(b"evidence")
        return assets, identity

    def test_seal_binds_nested_pair_source_commits(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            assets, identity = self._fixture(Path(directory))
            result = trusted.seal_release(assets, identity, Path(directory) / "sealed")
            self.assertEqual(
                result["identity"],
                trusted.paired_identity(identity["tag"], source_sha=identity["source"], built_at=identity["built_at"]),
            )
            pair = json.loads((assets / "smarty-pair.json").read_text(encoding="utf-8"))
            pair["sources"]["omp"]["commit"] = "4" * 40
            (assets / "smarty-pair.json").write_text(json.dumps(pair), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "pair manifest does not bind sealed source identity"):
                trusted.seal_release(assets, identity, Path(directory) / "sealed-bad")


class TrustedExistingReleaseTests(unittest.TestCase):
    parent = "1" * 40
    source = "2" * 40
    omp = "3" * 40

    def _fixture(
        self, root: Path,
    ) -> tuple[
        Path, dict[str, str], dict[str, object], list[dict[str, object]], dict[str, object],
    ]:
        tag = f"smarty-preview-2026-08-22-p{self.parent}-r{self.source}-o{self.omp}"
        identity = {
            "tag": tag,
            "build_id": tag.removeprefix("smarty-preview-"),
            "built_at": "2026-08-22T00:00:00Z",
            "parent": self.parent,
            "source": self.source,
            "omp": self.omp,
        }
        asset_dir = root / "assets"
        asset_dir.mkdir()
        for name in trusted.FULL_RELEASE_ASSETS:
            (asset_dir / name).write_bytes(f"published:{name}".encode())
        release_id = 42
        release = {
            "id": release_id,
            "tag_name": tag,
            "name": tag,
            "body": "Trusted paired preview release",
            "draft": False,
            "prerelease": True,
            "immutable": True,
            "url": f"https://api.github.com/repos/{trusted.REPOSITORY}/releases/{release_id}",
            "html_url": f"https://github.com/{trusted.REPOSITORY}/releases/tag/{tag}",
            "assets_url": f"https://api.github.com/repos/{trusted.REPOSITORY}/releases/{release_id}/assets",
        }
        api_assets = []
        for asset_id, name in enumerate(sorted(trusted.FULL_RELEASE_ASSETS), start=100):
            record = trusted.file_record(asset_dir / name)
            api_assets.append({
                "id": asset_id,
                "name": name,
                "state": "uploaded",
                "size": record["length"],
                "digest": f"sha256:{record['sha256']}",
                "url": f"https://api.github.com/repos/{trusted.REPOSITORY}/releases/assets/{asset_id}",
                "browser_download_url": f"https://github.com/{trusted.REPOSITORY}/releases/download/{tag}/{name}",
            })
        tag_object = {"ref": f"refs/tags/{tag}", "object": {"type": "commit", "sha": self.source}}
        return asset_dir, identity, release, api_assets, tag_object

    def test_validator_accepts_remote_bytes_without_fresh_digest_comparison(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            asset_dir, identity, release, api_assets, tag_object = self._fixture(Path(directory))
            metadata = trusted.validate_existing_release_metadata(release, api_assets, identity)
            self.assertEqual(metadata["assets"]["smarty-pair.json"]["id"], next(
                item["id"] for item in api_assets if item["name"] == "smarty-pair.json"
            ))
            result = trusted.validate_existing_release(release, api_assets, tag_object, identity, asset_dir)
            expected_identity = trusted.paired_identity(
                identity["tag"], source_sha=self.source, built_at=identity["built_at"],
            )
            self.assertEqual(result["identity"], {**expected_identity, "built_at": identity["built_at"]})
            self.assertEqual(
                result["files"]["herdr-windows-x86_64.zip"],
                trusted.file_record(asset_dir / "herdr-windows-x86_64.zip"),
            )
            fresh_digest = "f" * 64
            self.assertNotEqual(result["files"]["herdr-windows-x86_64.zip"]["sha256"], fresh_digest)

    def test_validator_rejects_release_or_tag_identity_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            asset_dir, identity, release, api_assets, tag_object = self._fixture(Path(directory))
            for field, value in (("body", "other"), ("draft", True), ("prerelease", False), ("immutable", False)):
                candidate = dict(release)
                candidate[field] = value
                with self.subTest(field=field), self.assertRaises(ValueError):
                    trusted.validate_existing_release(candidate, api_assets, tag_object, identity, asset_dir)
            moved_tag = json.loads(json.dumps(tag_object))
            moved_tag["object"]["sha"] = "4" * 40
            with self.assertRaisesRegex(ValueError, "lightweight ref directly at R"):
                trusted.validate_existing_release(release, api_assets, moved_tag, identity, asset_dir)

    def test_validator_rejects_inventory_metadata_and_byte_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            asset_dir, identity, release, api_assets, tag_object = self._fixture(Path(directory))
            cases = []
            cases.append(("missing", api_assets[1:]))
            cases.append(("duplicate", [*api_assets, dict(api_assets[0])]))
            bad_digest = json.loads(json.dumps(api_assets))
            bad_digest[0]["digest"] = f"sha256:{'f' * 64}"
            cases.append(("digest", bad_digest))
            bad_size = json.loads(json.dumps(api_assets))
            bad_size[0]["size"] += 1
            cases.append(("size", bad_size))
            for label, candidate in cases:
                with self.subTest(label=label), self.assertRaises(ValueError):
                    trusted.validate_existing_release(release, candidate, tag_object, identity, asset_dir)
            target = asset_dir / trusted.FULL_RELEASE_ASSETS[0]
            target.unlink()
            target.symlink_to(asset_dir / trusted.FULL_RELEASE_ASSETS[1])
            with self.assertRaisesRegex(ValueError, "37-file allow-list"):
                trusted.validate_existing_release(release, api_assets, tag_object, identity, asset_dir)

    def test_validator_enforces_aggregate_bound(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            asset_dir, identity, release, api_assets, tag_object = self._fixture(Path(directory))
            with mock.patch.object(trusted, "MAX_TOTAL_DOWNLOAD_BYTES", 1):
                with self.assertRaisesRegex(ValueError, "aggregate size"):
                    trusted.validate_existing_release_metadata(release, api_assets, identity)


class TrustedWorkflowTests(unittest.TestCase):
    workflow = Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview-publish.yml"

    def test_workflow_uses_exact_source_handoff_and_isolates_build(self) -> None:
        source = self.workflow.read_text(encoding="utf-8")
        self.assertNotIn("eval(", source)
        self.assertIn("omp-source.tar", source)
        self.assertIn("--event-run-attempt \"$EVENT_RUN_ATTEMPT\"", source)
        self.assertIn("validate-sources", source)
        self.assertIn("download-artifacts --root artifact-zips --identity identity.json", source)
        self.assertIn("extract-producer-artifacts --root artifact-zips --output producer --identity identity.json", source)
        self.assertIn("ref: ${{ needs.validate-seal.outputs.source }}", source)
        self.assertIn("token: ${{ github.token }}", source)
        self.assertIn('json.dumps(record["omp"]', source)
        trusted_source = source.split("\n  trusted-source:", 1)[1].split("\n  trusted-build:", 1)[0]
        trusted_build = source.split("\n  trusted-build:", 1)[1].split("\n  trusted-omp-build:", 1)[0]
        omp_build = source.split("\n  trusted-omp-build:", 1)[1].split("\n  trusted-assemble:", 1)[0]
        assemble = source.split("\n  trusted-assemble:", 1)[1].split("\n  attest-and-seal:", 1)[0]
        attest = source.split("\n  attest-and-seal:", 1)[1].split("\n  publish-release:", 1)[0]
        self.assertIn("needs: [validate-seal]", trusted_source)
        self.assertNotIn("trusted-build", trusted_source)
        self.assertIn("fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'", trusted_source)
        self.assertIn('git -C parent-source merge-base --is-ancestor "$PARENT" refs/remotes/origin/main', trusted_source)
        self.assertIn("tarfile.open", trusted_source)
        self.assertIn("runs-on: ${{ matrix.os }}", omp_build)
        self.assertIn("Build trusted Herdr source", trusted_build)
        self.assertNotIn("if: runner.os == 'Linux'", trusted_build)
        self.assertNotIn("publisher-source", trusted_build)
        self.assertIn("Download validated OMP identity handoff", omp_build)
        for job in (omp_build, assemble, attest):
            self.assertIn("Checkout exact private OMP source", job)
            self.assertIn("ref: ${{ needs.trusted-source.outputs.omp_commit }}", job)
            self.assertEqual(job.count("token: ${{ secrets.SMARTY_SOURCE_READ_TOKEN }}"), 1)
            self.assertIn('git -C omp-source rev-parse HEAD^{commit}', job)
            self.assertIn('git -C omp-source rev-parse HEAD^{tree}', job)
            self.assertIn('git -C omp-source status --porcelain=v1 --untracked-files=all', job)
        self.assertNotIn("extract-tar", omp_build)
        self.assertIn("trusted-tools/smarty_preview_trusted.py", omp_build)
        self.assertNotIn("Checkout exact Herdr source", omp_build)
        self.assertNotIn("HERDR_BUILD_OMP", omp_build)
        self.assertIn('(cd omp-source && bun scripts/ci-release-build-binaries.ts --targets "$OMP_TARGET")', omp_build)
        self.assertNotIn("bun run gen:bundle", omp_build)
        self.assertLess(omp_build.index("bun install --frozen-lockfile"), omp_build.index("bazel-natives.ts"))
        self.assertLess(omp_build.index("bazel-natives.ts"), omp_build.index("ci-release-build-binaries.ts"))
        self.assertIn('cp "omp-source/packages/coding-agent/binaries/omp-$OMP_TARGET" "release-assets/omp-$PLATFORM"', omp_build)
        self.assertIn('test "$(release-assets/omp-$PLATFORM __build-id)" = "$OMP_BUILD_ID"', omp_build)
        self.assertNotIn('bun build omp-source/packages/coding-agent/src/cli.ts --compile --outfile "release-assets/omp-$PLATFORM"', omp_build)
        for public_handoff in (trusted_source, omp_build, trusted_assemble):
            self.assertNotIn("omp-source.tar", public_handoff)
        self.assertIn("source-archives/herdr-source.tar", trusted_assemble)
        self.assertIn('Path("validation/producer-record.json")', trusted_assemble)
        self.assertNotIn('Path("validation/producer/producer-record.json")', trusted_assemble)
        self.assertIn("git -C omp-source archive --format=tar", attest)
        self.assertIn("validate-git-archive --archive source-archives/omp-source.tar", attest)
        self.assertIn("mod --repo_env=CARGO_BAZEL_ISOLATED=0 --repo_env=CARGO_BAZEL_TIMEOUT=1800 --lockfile_mode=off graph --output=json", trusted_assemble)
        self.assertEqual(source.count("omp-source.tar"), 2)

    def test_workflow_pins_executing_revision_and_publisher_attempt(self) -> None:
        source = self.workflow.read_text(encoding="utf-8")
        self.assertIn("ref: ${{ github.workflow_sha }}", source)
        self.assertIn("WORKFLOW_SHA: ${{ github.workflow_sha }}", source)
        self.assertIn("publisher_sha != workflow_sha", source)
        artifact_names = re.findall(
            r"uses: actions/(?:upload|download)-artifact@[^\n]+\n\s+with:\n\s+name: ([^\n]+)",
            source,
        )
        self.assertTrue(artifact_names)
        for name in artifact_names:
            self.assertTrue(
                "${{ github.run_attempt }}" in name or ".outputs.artifact_attempt }}" in name,
                name,
            )

    def test_workflow_promotes_internal_preview_with_protected_channel_cas(self) -> None:
        workflow = load_workflow(self.workflow)
        jobs = workflow["jobs"]
        phase_a = jobs["phase-a-channel"]
        phase_a_commit = jobs["phase-a-commit"]
        phase_b = jobs["phase-b-promotion"]
        phase_b_commit = jobs["phase-b-commit"]
        self.assertEqual(phase_a["permissions"]["contents"], "read")
        self.assertEqual(phase_a_commit["permissions"]["contents"], "write")
        self.assertEqual(phase_a_commit["environment"], "smarty-release")
        self.assertEqual(phase_b["permissions"]["contents"], "read")
        self.assertEqual(phase_b["environment"], {"name": "smarty-preview-promotion"})
        self.assertIn("phase-a-commit", phase_b["needs"])
        self.assertEqual(phase_b_commit["permissions"]["contents"], "write")
        self.assertIn("phase-b-promotion", phase_b_commit["needs"])
        source = self.workflow.read_text(encoding="utf-8")
        promotion_source = source.split("\n  phase-a-channel:", 1)[1]
        for forbidden in ("awaiting-external-tuf-signing", "blocked-external-gate", "trusted-tuf-promotion-gate", "SMARTY_RELEASE_TOKEN"):
            self.assertNotIn(forbidden, promotion_source)
        self.assertEqual(promotion_source.count("smarty_preview_promotion.py"), 4)
        self.assertNotIn("push --atomic", promotion_source)
        self.assertNotIn("git -C", promotion_source)
        self.assertIn("trusted-channel-promotion-authorization-", source)
        self.assertIn("--producer-attempt", promotion_source)
        self.assertIn("${{ github.run_attempt }}", promotion_source)
        self.assertIn("trusted-channel-bridge-${{ needs.validate-seal.outputs.tag }}-${{ needs.validate-seal.outputs.run_attempt }}-${{ github.run_attempt }}", promotion_source)
        self.assertIn("trusted-channel-promotion-authorization-${{ needs.validate-seal.outputs.tag }}-${{ needs.validate-seal.outputs.run_attempt }}-${{ github.run_attempt }}", promotion_source)

    def test_draft_release_download_uses_auth_stripping_redirects(self) -> None:
        source = self.workflow.read_text(encoding="utf-8")
        publish = source.split("\n  publish-release:", 1)[1]
        self.assertIn("Checkout trusted publisher source", publish)
        self.assertIn(
            "ref: ${{ needs.validate-seal.outputs.publisher_commit }}", publish
        )
        self.assertIn("path: publisher-source", publish)
        self.assertIn(
            "from smarty_preview_trusted import _HttpsArtifactRedirectHandler", publish
        )
        self.assertIn("build_opener(_HttpsArtifactRedirectHandler())", publish)
        self.assertIn("with opener.open(request, timeout=120)", publish)
        self.assertNotIn("with urlopen(request", publish)
    def test_workflow_reuses_only_verified_immutable_release_bytes(self) -> None:
        workflow = load_workflow(self.workflow)
        jobs = workflow["jobs"]
        attest_steps = jobs["attest-and-seal"]["steps"]
        by_name = {step["name"]: step for step in attest_steps}
        reuse = by_name["Reuse verified immutable release bytes"]
        run = reuse["run"]
        reuse_index = next(
            index for index, step in enumerate(attest_steps)
            if step["name"] == "Reuse verified immutable release bytes"
        )
        attest_index = next(
            index for index, step in enumerate(attest_steps)
            if str(step.get("uses", "")).startswith("actions/attest@")
        )
        self.assertLess(reuse_index, attest_index)
        self.assertEqual(reuse["if"], "steps.existing.outputs.reuse == 'true'")
        self.assertEqual(reuse["env"]["CERT_IDENTITY"], trusted.PUBLISH_CERT_IDENTITY)
        self.assertEqual(reuse["env"]["SOURCE_REF"], trusted.PUBLISH_SOURCE_REF)
        self.assertLess(
            run.index("gh attestation verify immutable-assets/smarty-pair.json"),
            run.index("verify-attested-pair"),
        )
        self.assertLess(run.index("validate-pair-attestation"), run.index("verify-attested-pair"))
        self.assertLess(
            run.index("validate-existing-release-metadata"),
            run.index("Accept: application/octet-stream"),
        )
        for text in (
            "validate-existing-release",
            "validate-existing-release-metadata",
            "Accept: application/octet-stream",
            "releases/assets/$asset_id",
            "verify-attested-pair",
            "gh attestation verify",
            '--bundle "immutable-assets/$bundle"',
            '--repo "$REPOSITORY"',
            '--cert-identity "$CERT_IDENTITY"',
            '--source-ref "$SOURCE_REF"',
            "--predicate-type https://slsa.dev/provenance/v1",
            "--deny-self-hosted-runners",
            "--format json > existing-pair-attestation.json",
            "validate-pair-attestation",
            "Accept: application/vnd.github.raw+json",
            "?ref=$attested_publisher",
            "--trusted-verifier attested-trusted-verifier.py",
            "--asset-dir immutable-assets --identity existing-identity.json --output-dir final-seal",
            "mv immutable-assets release-assets",
        ):
            self.assertIn(text, run)
        self.assertNotIn("--signer-workflow", run)
        self.assertNotIn("gh release download", run)
        self.assertEqual(run.count("smarty-pair.provenance.sigstore.json"), 1)
        for forbidden in ("gh release create", "gh release upload", "gh release edit", "--method DELETE"):
            self.assertNotIn(forbidden, run)
        fresh_steps = (
            "Write platform and SBOM subject lists",
            "Attest Linux x86_64 release subjects",
            "Save Linux x86_64 provenance bundle",
            "Attest Linux aarch64 release subjects",
            "Save Linux aarch64 provenance bundle",
            "Attest macOS x86_64 release subjects",
            "Save macOS x86_64 provenance bundle",
            "Attest macOS aarch64 release subjects",
            "Save macOS aarch64 provenance bundle",
            "Attest Windows x86_64 release subject",
            "Save Windows x86_64 provenance bundle",
            "Attest SPDX release subjects",
            "Save SPDX provenance bundle",
            "Build and verify canonical paired manifest",
            "Attest paired manifest",
            "Save paired provenance bundle and seal release",
        )
        for name in fresh_steps:
            self.assertEqual(by_name[name]["if"], "steps.existing.outputs.reuse != 'true'", name)
        self.assertNotIn("if", by_name["Upload final immutable release bytes and seal"])


def load_tests(loader: unittest.TestLoader, tests: unittest.TestSuite, pattern: str | None) -> unittest.TestSuite:
    tests.addTests(loader.loadTestsFromModule(promotion_tests))
    return tests
if __name__ == "__main__":
    unittest.main()
