"""Training failure categories and transport error classification."""

from __future__ import annotations


class ConfigurationError(RuntimeError):
    pass


_TRANSIENT_TYPES = (OSError, TimeoutError, ConnectionError)

_TRANSIENT_NAMES = {
    "ConnectTimeoutError",
    "ReadTimeoutError",
    "IncompleteReadError",
    "ProtocolError",
    "ChunkedEncodingError",
    "HfHubHTTPError",
    "RemoteDisconnected",
    "SSLError",
    "MaxRetryError",
    "NewConnectionError",
    "NameResolutionError",
    "ConnectError",
    "ConnectTimeout",
    "ReadTimeout",
    "WriteTimeout",
    "PoolTimeout",
    "ReadError",
    "WriteError",
    "RemoteProtocolError",
    "ProxyError",
}

# a dropped connection can leave the shared hub http client closed
_TRANSIENT_RUNTIME_MARKS = ("client has been closed",)


def is_transient(error: BaseException) -> bool:
    """Whether an error is a retryable transport failure."""

    seen: set[int] = set()
    current: BaseException | None = error
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        if _transient_link(current):
            return True
        current = current.__cause__ or current.__context__
    return False


def _transient_link(error: BaseException) -> bool:
    if isinstance(error, _TRANSIENT_TYPES):
        return True
    if type(error) is RuntimeError:
        return any(mark in str(error) for mark in _TRANSIENT_RUNTIME_MARKS)
    return type(error).__name__ in _TRANSIENT_NAMES
