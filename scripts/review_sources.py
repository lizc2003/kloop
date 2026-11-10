"""复核用只读证据：固定 Git blob 与校验 Cargo.lock 后的本机 crate 归档。"""

import copy
import hashlib
import io
import json
import os
import pathlib
import re
import subprocess
import tarfile
import tomllib

import review_recheck as review


def relative_path(value, *, allow_empty=False):
    if not isinstance(value, str) or "\0" in value:
        raise ValueError("invalid relative path")
    path = pathlib.PurePosixPath(value)
    if path.is_absolute() or any(part in ("..", ".git") for part in path.parts):
        raise ValueError("path must stay inside the frozen source")
    if str(path) == ".":
        if allow_empty:
            return ""
        raise ValueError("source file path is required")
    return str(path)


class Sources:
    def __init__(self, source, packet, *, cargo_home=None, max_chars=160_000):
        self.source = pathlib.Path(source)
        target = review.git(source, "rev-parse", "--verify", f"{packet['target']}^{{commit}}").strip()
        baseline = review.git(source, "rev-parse", f"{target}^").strip()
        if target != packet["target"] or baseline != packet["baseline"]:
            raise ValueError("packet revisions do not match the frozen commit and parent")
        self.revisions = {"before": baseline, "after": target}
        self.candidates = {c["id"] for c in packet["candidates"]}
        self.cargo_home = pathlib.Path(cargo_home or os.environ.get("CARGO_HOME", pathlib.Path.home() / ".cargo"))
        self.max_chars, self.chars = max_chars, 0
        self.evidence, self.grants, self.receipts = [], {}, []
        self._trees, self._blobs, self._pins, self._archives = {}, {}, {}, {}

    def tree(self, version):
        revision = self.revisions[version]
        if version not in self._trees:
            raw = subprocess.run(["git", "-C", str(self.source), "ls-tree", "-rz", "--full-tree", revision],
                                 check=True, capture_output=True).stdout
            rows = {}
            for entry in raw.split(b"\0"):
                if entry:
                    metadata, path = entry.split(b"\t", 1)
                    mode, kind, oid = metadata.decode().split()
                    if kind == "blob":
                        rows[path.decode()] = (mode, oid)
            self._trees[version] = rows
        return self._trees[version]

    def blob(self, version, path):
        path = relative_path(path)
        key = (version, path)
        if key not in self._blobs:
            entry = self.tree(version).get(path)
            if entry is None:
                raise ValueError("path is not a frozen Git blob")
            # 符号链接只读 Git 中记录的链接文本，不沿宿主文件系统跟随。
            self._blobs[key] = review.git(self.source, "show", entry[1])
            if "\0" in self._blobs[key]:
                del self._blobs[key]
                raise ValueError("binary Git blob is not readable source text")
        return self._blobs[key]

    def pins(self, version):
        if version not in self._pins:
            pins = {}
            for path in self.tree(version):
                if pathlib.PurePosixPath(path).name != "Cargo.lock":
                    continue
                for package in tomllib.loads(self.blob(version, path)).get("package", []):
                    if "source" in package:
                        identity = f"{package['name']}@{package['version']}"
                        pin = {key: package.get(key) for key in ("name", "version", "source", "checksum")}
                        if identity in pins and pins[identity] != pin:
                            raise ValueError("ambiguous dependency identity across frozen lockfiles")
                        pins[identity] = pin
            self._pins[version] = pins
        return self._pins[version]

    def package_pin(self, version, package):
        matches = [p for identity, p in self.pins(version).items()
                   if package in (identity, p["name"])]
        if len(matches) != 1:
            raise ValueError("dependency missing or ambiguous; use source_packages and name@version")
        pin = matches[0]
        if (not isinstance(pin["source"], str) or not pin["source"].startswith("registry+")
                or not isinstance(pin["checksum"], str)):
            raise ValueError("dependency is not pinned to a checksummed registry archive")
        if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", pin["name"])
                or not re.fullmatch(r"[0-9A-Za-z.+-]+", pin["version"])
                or not re.fullmatch(r"[0-9a-f]{64}", pin["checksum"])):
            raise ValueError("invalid package identity or archive checksum in lockfile")
        return pin

    def archive(self, version, package):
        pin = self.package_pin(version, package)
        key = (pin["name"], pin["version"], pin["checksum"])
        if key not in self._archives:
            filename = f"{pin['name']}-{pin['version']}.crate"
            candidates = sorted((self.cargo_home / "registry/cache").glob("*/" + filename))
            verified = None
            for path in candidates:
                if path.stat().st_size > 32 << 20:
                    continue
                data = path.read_bytes()
                if hashlib.sha256(data).hexdigest() == pin["checksum"]:
                    verified = data
                    break
            if verified is None:
                raise ValueError("no local crate archive matches the frozen lockfile checksum")
            prefix = f"{pin['name']}-{pin['version']}/"
            with tarfile.open(fileobj=io.BytesIO(verified), mode="r:gz") as archive:
                members = archive.getmembers()
                if len(members) > 20_000 or sum(m.size for m in members) > 64 << 20:
                    raise ValueError("dependency archive exceeds inspection budget")
                files = {}
                for member in members:
                    if member.isfile() and member.name.startswith(prefix):
                        path = relative_path(member.name[len(prefix):])
                        if path in files:
                            raise ValueError("duplicate source path in dependency archive")
                        files[path] = archive.extractfile(member).read()
            self._archives[key] = files
            self.receipts.append({"package": f"{pin['name']}@{pin['version']}",
                                  "archive_sha256": pin["checksum"]})
        return pin, self._archives[key]

    def content(self, version, package, path):
        path = relative_path(path)
        if not package:
            return self.blob(version, path), {"revision": self.revisions[version]}
        pin, files = self.archive(version, package)
        if path not in files:
            raise ValueError("file is absent from the verified dependency archive")
        if b"\0" in files[path]:
            raise ValueError("binary archive member is not readable source text")
        return files[path].decode(), {"revision": self.revisions[version],
                                     "package": f"{pin['name']}@{pin['version']}",
                                     "archive_sha256": pin["checksum"]}

    def register(self, candidate, version, package, path, start, end):
        text, origin = self.content(version, package, path)
        lines = text.splitlines()
        if type(start) is not int or type(end) is not int or start < 1 or end < start or end > len(lines):
            raise ValueError("requested lines are outside the frozen source")
        if end - start + 1 > 200:
            raise ValueError("read budget is at most 200 lines; narrow the range")
        key = (version, package, path, start, end)
        existing = next((e for e in self.evidence if e["key"] == key), None)
        if existing is None:
            numbered = "\n".join(f"{n}: {lines[n - 1]}" for n in range(start, end + 1))
            if len(numbered) > 24_000 or self.chars + len(numbered) > self.max_chars:
                raise ValueError("additional source text budget exceeded")
            existing = {"id": f"S{len(self.evidence) + 1}", "key": key, "version": version,
                        "path": path, "start": start, "end": end, "text": numbered,
                        "blob_sha256": hashlib.sha256(text.encode()).hexdigest(), **origin}
            self.evidence.append(existing)
            self.chars += len(numbered)
        self.grants.setdefault(candidate, set()).add(existing["id"])
        return {key: value for key, value in existing.items() if key != "key"}

    def search(self, candidate, version, package, query, prefix):
        if not isinstance(query, str) or not query or len(query) > 128 or "\n" in query or "\0" in query:
            raise ValueError("search requires a literal query of 1..128 characters on one line")
        prefix = relative_path(prefix, allow_empty=True)
        matches = []
        if package:
            _, files = self.archive(version, package)
            for path, raw in sorted(files.items()):
                if not path.startswith(prefix) or b"\0" in raw:
                    continue
                try:
                    lines = raw.decode().splitlines()
                except UnicodeDecodeError:
                    continue
                for number, line in enumerate(lines, 1):
                    if query in line:
                        matches.append((path, number, line))
                        if len(matches) > 12:
                            break
                if len(matches) > 12:
                    break
        else:
            command = ["git", "-C", str(self.source), "grep", "-n", "-z", "-I", "-F", "-m", "13",
                       "-e", query, self.revisions[version], "--"]
            if prefix:
                command.append(f":(literal){prefix}")
            result = subprocess.run(command, capture_output=True, check=False)
            if result.returncode not in (0, 1):
                raise ValueError("frozen source search failed")
            if len(result.stdout) > 16 << 20:
                raise ValueError("search capture budget exceeded; narrow the path")
            remaining = result.stdout
            while remaining and len(matches) <= 12:
                path, number, rest = remaining.split(b"\0", 2)
                line, _, remaining = rest.partition(b"\n")
                path = path.decode().removeprefix(self.revisions[version] + ":")
                matches.append((path, int(number), line.decode()))
        sources, locations = [], []
        for path, number, line in matches[:12]:
            lines = self.content(version, package, path)[0].splitlines()
            evidence = self.register(candidate, version, package, path, max(1, number - 2), min(len(lines), number + 2))
            sources.append(evidence)
            locations.append({"source": evidence["id"], "path": path, "line": number, "text": line})
        return {"status": "ok", "matches": locations, "sources": sources, "truncated": len(matches) > 12}

    def call(self, name, args):
        before = len(self.evidence), self.chars, copy.deepcopy(self.grants)
        try:
            candidate, version = args["candidate_id"], args["version"]
            if candidate not in self.candidates or version not in self.revisions:
                raise ValueError("unknown candidate or source version")
            if name == "source_packages":
                query = args.get("query", "")
                if not isinstance(query, str) or len(query) > 128:
                    raise ValueError("invalid dependency name query")
                pins = [{"package": identity, "source": pin["source"], "checksum": pin["checksum"]}
                        for identity, pin in sorted(self.pins(version).items()) if query in identity]
                return {"status": "ok", "packages": pins[:20], "truncated": len(pins) > 20}
            package, path = args.get("package", ""), args.get("path", "")
            if not isinstance(package, str):
                raise ValueError("invalid dependency identity")
            if name == "source_read":
                evidence = self.register(candidate, version, package, relative_path(path), args["start"], args["end"])
                return {"status": "ok", "sources": [evidence]}
            if name == "source_search":
                return self.search(candidate, version, package, args["query"], path)
            raise ValueError("unknown read-only evidence tool")
        except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError, tarfile.TarError) as error:
            count, self.chars, self.grants = before
            del self.evidence[count:]
            # 此处不读鉴权；只转达本地证据状态，不包含异常对象的命令或宿主路径。
            return {"status": "unavailable", "error_type": type(error).__name__,
                    "missing": str(error) if isinstance(error, ValueError) else "frozen source unavailable"}

    def validation_packet(self, packet):
        extended = copy.deepcopy(packet)
        payload = json.loads(packet["prompt"][len(review.PROMPT):])
        evidence = [{key: value for key, value in row.items() if key != "key"} for row in self.evidence]
        extended["evidence"].extend(evidence)
        payload["evidence"].extend(evidence)
        for candidate in payload["candidates"]:
            candidate["evidence"].extend(sorted(self.grants.get(candidate["id"], set())))
        extended["prompt"] = review.PROMPT + json.dumps(payload, ensure_ascii=False)
        extended["prompt_chars"] = len(extended["prompt"])
        extended["prompt_sha256"] = review.digest(extended["prompt"])
        extended.pop("packet_sha256", None)
        extended["packet_sha256"] = review.digest(json.dumps(extended, ensure_ascii=False, sort_keys=True))
        return extended
