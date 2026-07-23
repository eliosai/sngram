"""Training failure categories and transport error classification."""

from __future__ import annotations


class ConfigurationError(RuntimeError):
    pass


class CorpusExhausted(RuntimeError):
    pass


_TRANSIENT_TYPES = (OSError, TimeoutError, ConnectionError)

_TRANSIENT_NAMES = {
    "EndpointConnectionError",
    "ConnectTimeoutError",
    "ReadTimeoutError",
    "ResponseStreamingError",
    "IncompleteReadError",
    "ProtocolError",
    "ChunkedEncodingError",
    "HfHubHTTPError",
    "RemoteDisconnected",
    "SSLError",
    "MaxRetryError",
    "NewConnectionError",
    "NameResolutionError",
}

_TRANSIENT_S3_CODES = {
    "SlowDown",
    "Throttling",
    "ThrottlingException",
    "RequestTimeout",
    "InternalError",
    "ServiceUnavailable",
    "RequestLimitExceeded",
    "500",
    "502",
    "503",
    "504",
}


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
    if type(error).__name__ == "ClientError":
        return _client_error_code(error) in _TRANSIENT_S3_CODES
    if isinstance(error, _TRANSIENT_TYPES):
        return True
    return type(error).__name__ in _TRANSIENT_NAMES


def _client_error_code(error: BaseException) -> str:
    response = getattr(error, "response", None) or {}
    return str(response.get("Error", {}).get("Code", ""))
