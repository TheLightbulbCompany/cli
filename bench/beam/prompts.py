"""ExaBase's BEAM prompts are fetched, never committed (see NOTICE.md).

ensure_exabase_prompts(cache_dir) downloads ExaBase's published adapter into cache_dir and checks
its SHA-256 against SOURCE_SHA256, the hash past.dev's verify.py expects. A mismatch raises.
cache_dir must be outside the repo tree (benchmarks/beam/.cache is git-ignored).

Once verified, the cached file is loaded as a module (never from the repo tree) and this module
re-exports what past.dev's exabase_prompts.py exposed: answer_prompt, rubric_item_prompt,
equivalence_prompt, RUBRIC_SCHEMA, EQUIVALENCE_SCHEMA. They are derived by calling ExaBase's own
code, so no ExaBase text lives in this repo. Names load lazily: importing this module needs no cache.
"""

import hashlib
import importlib.util
import os
import re
import sys
import types
from pathlib import Path

import httpx

SOURCE_URL = "https://fabric.so/p/beam-3VjBcqEVRofZyA5CeazeX"
SOURCE_SHA256 = "584b672d957f3e6f75e96f881ec31da22e1039da1df5fbedb53dc6a9145cfc43"
CACHED_NAME = "exabase_beam_adapter.py"


class PromptsHashMismatch(RuntimeError):
    pass


def download_exabase_adapter() -> bytes:
    """The published page links the file through a signed download URL; follow it."""
    page = httpx.get(SOURCE_URL, follow_redirects=True, timeout=30).text.replace("\\u0026", "&")
    link = re.search(r'https://cdn\.fabric\.so/[^"\s]+?\.py\?token=[^"\s\\]+', page)
    if link is None:
        raise RuntimeError(f"Could not find the adapter's download link on {SOURCE_URL}")
    return httpx.get(link.group(0), follow_redirects=True, timeout=30).content


def ensure_exabase_prompts(cache_dir, fetch=download_exabase_adapter) -> Path:
    """Path of the verified ExaBase file in cache_dir, downloading it first if absent. Also
    points the lazy exports at this cache."""
    global _cache_dir, _loaded
    if Path(cache_dir) != _cache_dir:
        _cache_dir, _loaded = Path(cache_dir), None
    path = Path(cache_dir) / CACHED_NAME
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(fetch())
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != SOURCE_SHA256:
        raise PromptsHashMismatch(f"{path} hashes to {digest}, expected {SOURCE_SHA256}")
    return path


# --- Loading the verified file -----------------------------------------------------------------

DEFAULT_CACHE_DIR = Path(os.environ.get("BEAM_PROMPTS_CACHE", Path(__file__).parent / ".cache"))
_cache_dir = DEFAULT_CACHE_DIR
_loaded: dict | None = None
EXPORTS = ("answer_prompt", "rubric_item_prompt", "equivalence_prompt", "RUBRIC_SCHEMA", "EQUIVALENCE_SCHEMA")


class _Stub(types.ModuleType):
    """Stands in for the ExaBase packages the adapter imports; any name is an empty class."""

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        cls = type(name, (), {})
        setattr(self, name, cls)
        return cls


class _Schema:
    def __init__(self, **kwargs):
        self.fields = kwargs


class _Capture:
    """Records the prompt and schema ExaBase's code hands to its LLM."""

    def __init__(self):
        self.prompt = self.schema = None

    def generate(self, prompt, schema):
        self.prompt, self.schema = prompt, schema.fields
        return {}


def _load(path: Path) -> dict:
    pkg = "_exabase_beam"
    names = [pkg, f"{pkg}.datasets", f"{pkg}.datasets._cache", f"{pkg}.datasets.base",
             f"{pkg}.models", f"{pkg}.llm", f"{pkg}.llm.base", "rich", "rich.console", "rich.table"]
    have_rich = importlib.util.find_spec("rich") is not None
    saved = {n: sys.modules.get(n) for n in names + [f"{pkg}.datasets.beam"]}
    for n in names:
        if n.startswith("rich") and have_rich:
            continue
        sys.modules[n] = _Stub(n)
        sys.modules[n].__path__ = []
    sys.modules[f"{pkg}.llm.base"].Schema = _Schema
    try:
        spec = importlib.util.spec_from_file_location(f"{pkg}.datasets.beam", path)
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
    finally:
        for n, old in saved.items():
            if old is None:
                sys.modules.pop(n, None)
            else:
                sys.modules[n] = old
    dataset = module.BEAMDataset

    def answer_prompt(query: str, context: str, cat: str, meta: dict) -> str:
        return dataset.build_rag_prompt(None, query, context, "open", "", category=cat, meta=meta)

    def rubric_item_prompt(query: str, answer: str, rubric_item: str) -> str:
        seen = _Capture()
        dataset._rubric_item_score(query, answer, rubric_item, seen)
        return seen.prompt

    def equivalence_prompt(ref_item: str, sys_item: str) -> str:
        seen = _Capture()
        dataset._llm_equivalence(ref_item, sys_item, seen)
        return seen.prompt

    rubric, equivalence = _Capture(), _Capture()
    dataset._rubric_item_score("q", "a", "r", rubric)
    dataset._llm_equivalence("a", "b", equivalence)
    return {"answer_prompt": answer_prompt, "rubric_item_prompt": rubric_item_prompt,
            "equivalence_prompt": equivalence_prompt,
            "RUBRIC_SCHEMA": rubric.schema, "EQUIVALENCE_SCHEMA": equivalence.schema}


def _no_download():
    raise RuntimeError(f"ExaBase's prompts are not in {_cache_dir}; call "
                       f"prompts.ensure_exabase_prompts({str(_cache_dir)!r}) first to download and verify them")


def __getattr__(name):
    global _loaded
    if name not in EXPORTS:
        raise AttributeError(name)
    if _loaded is None:
        _loaded = _load(ensure_exabase_prompts(_cache_dir, fetch=_no_download))
    return _loaded[name]
