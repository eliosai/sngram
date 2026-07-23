from sngram_train.config import hf_token


def test_hf_token_uses_project_environment(monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "token")
    assert hf_token() == "token"
