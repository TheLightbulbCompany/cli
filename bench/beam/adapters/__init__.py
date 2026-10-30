"""Memory backends by name: `--backend fake | free`."""

import importlib

BACKENDS = {"fake": "adapters.fake:FakeBackend", "free": "adapters.free_cli:FreeCliBackend"}


def load(name: str, **options):
    if name not in BACKENDS:
        raise SystemExit(f"unknown backend {name!r}; choose one of {', '.join(BACKENDS)}")
    module, cls = BACKENDS[name].split(":")
    return getattr(importlib.import_module(module), cls)(**options)
