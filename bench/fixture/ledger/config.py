_SETTINGS: dict = {}


def get(key: str, default=None):
    return _SETTINGS.get(key, default)


def load(settings: dict) -> None:
    _SETTINGS.update(settings)
