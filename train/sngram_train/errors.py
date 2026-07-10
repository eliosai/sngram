"""Training failure categories."""


class ConfigurationError(RuntimeError):
    pass


class CorpusExhausted(RuntimeError):
    pass
