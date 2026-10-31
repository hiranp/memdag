import json
import urllib.request


def get(url: str) -> dict:
    with urllib.request.urlopen(url) as resp:
        return json.load(resp)


def request_with_retry(url: str, retries: int = 3, timeout: float = 5.0) -> dict:
    last = None
    for _ in range(retries):
        try:
            with urllib.request.urlopen(url, timeout=timeout) as resp:
                return json.load(resp)
        except OSError as e:
            last = e
    raise last
