"""Download Microsoft's Azure DevOps specs from the Software Heritage archive, verified by git blob hash.

The GitHub repository MicrosoftDocs/vsts-rest-api-specs is gone, so the files come from the archive's copy of it.
`pins.json` records the git blob hash of every file the generator uses, so a normal run downloads only those files
(one request each) and refuses anything whose hash differs. Downloads are cached by hash and never go stale.
"""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

from . import __version__

API = "https://archive.softwareheritage.org/api/1"
# The archive answers 403 to requests without an identifying User-Agent.
USER_AGENT = f"azdo-manuals/{__version__} (+https://github.com/Faraclas/agent-misc)"
PINS_FILE = Path(__file__).resolve().parent.parent / "pins.json"


class ArchiveError(Exception):
    pass


def git_blob_sha1(data: bytes) -> str:
    """The hash git (and the archive's `sha1_git`) gives a file's content."""
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def default_cache_dir() -> Path:
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA") or str(Path.home() / "AppData" / "Local")
        return Path(base) / "azdo-manuals" / "cache"
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "azdo-manuals"


def load_pins(path: Path = PINS_FILE) -> dict:
    with open(path, encoding="utf-8") as f:
        return json.load(f)


class Archive:
    """Minimal client for the archive's public API (no account needed, 120 requests per hour)."""

    def __init__(self, api: str = API, opener=urllib.request.urlopen):
        self.api = api.rstrip("/")
        self.opener = opener
        self.requests = 0
        self.remaining = None

    def _get(self, path: str) -> bytes:
        req = urllib.request.Request(self.api + path, headers={"User-Agent": USER_AGENT})
        try:
            with self.opener(req, timeout=60) as resp:
                self.requests += 1
                self.remaining = resp.headers.get("X-RateLimit-Remaining", self.remaining)
                return resp.read()
        except urllib.error.HTTPError as e:
            self.requests += 1
            e.close()
            if e.code == 429:
                reset = e.headers.get("X-RateLimit-Reset")
                when = time.strftime("%H:%M", time.localtime(int(reset))) if reset and reset.isdigit() else "later"
                raise ArchiveError(
                    f"the archive's rate limit (120 requests per hour) is used up; try again after {when}"
                ) from None
            raise ArchiveError(f"archive request {path} failed: HTTP {e.code}") from None
        except urllib.error.URLError as e:
            raise ArchiveError(f"cannot reach the archive ({e.reason})") from None

    def get_json(self, path: str):
        return json.loads(self._get(path))

    def get_content(self, sha1_git: str) -> bytes:
        return self._get(f"/content/sha1_git:{sha1_git}/raw/")

    def lookup(self, directory: str, path: str) -> dict:
        """One entry (`type`, `target`, `length`) at `path` below `directory`."""
        return self.get_json(f"/directory/{directory}/{path.strip('/')}/")

    def listing(self, directory: str) -> list:
        return self.get_json(f"/directory/{directory}/")


def fetch_file(archive: Archive, pin: dict, cache_dir: Path) -> bytes:
    """Return a pinned file's bytes from the cache or the archive, refusing a hash mismatch."""
    sha = pin["sha1_git"]
    cached = cache_dir / f"{sha}.json"
    if cached.is_file():
        data = cached.read_bytes()
        if git_blob_sha1(data) == sha:
            return data
        cached.unlink()
    data = archive.get_content(sha)
    actual = git_blob_sha1(data)
    if actual != sha:
        raise ArchiveError(f"{pin['file']}: the archive sent content with hash {actual}, pinned {sha}")
    cache_dir.mkdir(parents=True, exist_ok=True)
    tmp = cached.with_suffix(f".tmp{os.getpid()}")
    tmp.write_bytes(data)
    os.replace(tmp, cached)
    return data


def load_spec(data: bytes) -> dict:
    # Some files start with a UTF-8 byte order mark.
    return json.loads(data.decode("utf-8-sig"))


def discover_pins(archive: Archive, revision: str, api_version: str, areas: list, existing=None, log=print,
                  save=None) -> dict:
    """List `specification/<area>/<api_version>/*.json` at `revision` and return their pins.

    Costs 3 requests plus 2 per area. `existing` (an earlier, partial result for the same revision and version)
    is reused, so a run cut short by the rate limit can continue where it stopped. Areas without that version are
    recorded under `missing` with the versions they do have. `save(result)` is called after each area.
    """
    if existing and (existing.get("revision"), existing.get("api_version")) == (revision, api_version):
        result = existing
    else:
        rev = archive.get_json(f"/revision/{revision}/")
        result = {
            "revision": revision,
            "revision_date": rev.get("date"),
            "revision_message": (rev.get("message") or "").splitlines()[0] if rev.get("message") else None,
            "specification_dir": archive.lookup(rev["directory"], "specification")["target"],
            "api_version": api_version,
            "areas": {},
            "missing": {},
        }
    pins, missing = result["areas"], result.setdefault("missing", {})
    todo = [a for a in areas if a not in pins and a not in missing]
    if not todo:
        return result
    area_dirs = {e["name"]: e for e in archive.listing(result["specification_dir"]) if e["type"] == "dir"}
    for area in todo:
        if area not in area_dirs:
            raise ArchiveError(f"area '{area}' is not in the archived specification folder")
        versions = archive.listing(area_dirs[area]["target"])
        version_dir = next((e for e in versions if e["name"] == api_version and e["type"] == "dir"), None)
        if version_dir is None:
            missing[area] = sorted(e["name"] for e in versions if e["type"] == "dir")
            log(f"  {area}: no {api_version} folder (has {', '.join(missing[area]) or 'nothing'})")
            if save:
                save(result)
            continue
        files = [e for e in archive.listing(version_dir["target"])
                 if e["type"] == "file" and e["name"].endswith(".json")]
        pins[area] = [{"file": e["name"], "sha1_git": e["target"], "length": e["length"]} for e in files]
        log(f"  {area}: {', '.join(p['file'] for p in pins[area]) or '(no json files)'}")
        if save:
            save(result)
    return result


def warn(msg: str):
    print(f"azdo-manuals: {msg}", file=sys.stderr)
