"""Bounded Software Heritage content reads."""

from __future__ import annotations

import gzip
import os
import threading
from pathlib import Path
from urllib.parse import urlparse

from .config import STACK_V2_CONTENT_PREFIX


class SwhContent:
    def __init__(self, prefix: str = STACK_V2_CONTENT_PREFIX, workers: int = 32) -> None:
        self.prefix = prefix.rstrip("/")
        self.workers = workers
        self._client_value = None
        self._client_lock = threading.Lock()

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
        return self._read_s3(url, max_bytes)

    def _read_s3(self, url: str, max_bytes: int) -> bytes:
        parsed = urlparse(url)
        try:
            response = self._client().get_object(
                Bucket=parsed.netloc, Key=parsed.path.lstrip("/")
            )
        except Exception as error:
            if _is_missing(error):
                raise FileNotFoundError(url) from error
            raise
        body = response["Body"]
        try:
            with gzip.GzipFile(fileobj=body) as handle:
                return _bounded_read(handle, max_bytes)
        finally:
            body.close()

    def _client(self):
        with self._client_lock:
            if self._client_value is None:
                self._client_value = self._new_client()
            return self._client_value

    def _new_client(self):
        import boto3
        from botocore import UNSIGNED
        from botocore.config import Config

        options = {
            "max_pool_connections": max(self.workers, 8),
            "retries": {"max_attempts": 8, "mode": "adaptive"},
        }
        if os.environ.get("SNG_SWH_ANONYMOUS", "1") != "0":
            options["signature_version"] = UNSIGNED
        config = Config(**options)
        region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
        return boto3.client("s3", region_name=region or "us-east-1", config=config)


def _bounded_read(handle, max_bytes: int) -> bytes:
    data = handle.read(max_bytes + 1)
    if len(data) > max_bytes:
        raise ValueError("content exceeds its declared metadata length")
    return data


def _is_missing(error: Exception) -> bool:
    response = getattr(error, "response", {})
    code = str(response.get("Error", {}).get("Code", ""))
    return code in {"404", "NoSuchKey", "NotFound"}
