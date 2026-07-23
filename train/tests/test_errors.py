import httpx

from sngram_train.errors import is_transient


def test_network_stack_failures_are_transient():
    assert is_transient(
        httpx.ConnectError("[Errno -3] Temporary failure in name resolution")
    )
    assert is_transient(httpx.ReadTimeout("timed out"))
    assert is_transient(httpx.RemoteProtocolError("peer closed connection"))
    assert is_transient(OSError("[Errno -3] Temporary failure in name resolution"))


def test_a_closed_http_client_is_transient():
    assert is_transient(
        RuntimeError("Cannot send a request, as the client has been closed.")
    )


def test_chained_transient_causes_are_found():
    try:
        try:
            raise httpx.ConnectError("name resolution")
        except httpx.ConnectError as inner:
            raise RuntimeError("stream read failed") from inner
    except RuntimeError as outer:
        chained = outer

    assert is_transient(chained)


def test_plain_errors_are_not_transient():
    assert not is_transient(RuntimeError("deterministic bug"))
    assert not is_transient(ValueError("bad value"))
