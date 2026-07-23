"""Bounded Software Heritage content reads."""

from __future__ import annotations

import gzip
import os
import threading
import zlib
from pathlib import Path
from urllib.parse import urlparse

from .config import STACK_V2_CONTENT_PREFIX

RETRYABLE_STATUSES = (429, 500, 502, 503, 504)


class SwhContent:
    def __init__(self, prefix: str = STACK_V2_CONTENT_PREFIX, workers: int = 32) -> None:
        self.prefix = prefix.rstrip("/")
        self.workers = workers
        self._transport_value = None
        self._transport_lock = threading.Lock()

    def read(self, blob_id: str, max_bytes: int) -> bytes:
        try:
            return self._read(blob_id, max_bytes)
        except (gzip.BadGzipFile, EOFError) as error:
            raise ValueError("content is not a complete gzip stream") from error

    def _read(self, blob_id: str, max_bytes: int) -> bytes:
        url = f"{self.prefix}/{blob_id}"
        if url.startswith("file://"):
            with gzip.open(Path(urlparse(url).path), "rb") as handle:
                return _bounded_read(handle, max_bytes)
        if not url.startswith("s3://"):
            raise ValueError(f"unsupported content prefix: {self.prefix}")
        parsed = urlparse(url)
        raw = self._transport().fetch(parsed.netloc, parsed.path.lstrip("/"))
        return _gunzip_bounded(raw, max_bytes)

    def _transport(self):
        transport = self._transport_value
        if transport is not None:
            return transport
        with self._transport_lock:
            if self._transport_value is None:
                self._transport_value = _AnonymousTransport(self.workers)
            return self._transport_value


class _AnonymousTransport:
    """Unsigned virtual-hosted S3 GETs over one pooled HTTPS client."""

    def __init__(self, workers: int) -> None:
        import urllib3

        self._region = _region()
        self._pool = urllib3.PoolManager(
            maxsize=max(workers, 8),
            retries=urllib3.util.Retry(
                total=8, backoff_factor=0.5, status_forcelist=RETRYABLE_STATUSES
            ),
            timeout=urllib3.Timeout(connect=10.0, read=30.0),
        )

    def fetch(self, bucket: str, key: str) -> bytes:
        host = f"{bucket}.s3.{self._region}.amazonaws.com" if self._region else f"{bucket}.s3.amazonaws.com"
        response = self._pool.request("GET", f"https://{host}/{key}")
        if response.status == 404:
            raise FileNotFoundError(f"s3://{bucket}/{key}")
        if response.status != 200:
            raise ValueError(f"content GET returned status {response.status}")
        return response.data


def _region() -> str | None:
    return os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")


def _bounded_read(handle, max_bytes: int) -> bytes:
    data = handle.read(max_bytes + 1)
    if len(data) > max_bytes:
        raise ValueError("content exceeds its declared metadata length")
    return data


def _gunzip_bounded(raw: bytes, max_bytes: int) -> bytes:
    stream = zlib.decompressobj(wbits=31)
    try:
        data = stream.decompress(raw, max_bytes + 1)
    except zlib.error as error:
        raise ValueError("content is not a complete gzip stream") from error
    if len(data) > max_bytes:
        raise ValueError("content exceeds its declared metadata length")
    if not stream.eof:
        raise ValueError("content is not a complete gzip stream")
    return data
