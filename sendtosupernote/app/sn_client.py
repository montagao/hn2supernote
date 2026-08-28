"""Supernote Cloud client with the CSRF flow required by current APIs."""

from __future__ import annotations

from typing import Any

import httpx
from sncloud import SNClient
from sncloud.exceptions import ApiError

DEFAULT_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/107.0.0.0 Safari/537.36"
)


class SNClientWithCSRF(SNClient):
    """Add the CSRF cookie and header that Supernote requires for POSTs."""

    def __init__(self) -> None:
        super().__init__()
        self._client.headers.update({"User-Agent": DEFAULT_USER_AGENT})
        self._csrf_token: str | None = None

    def _fetch_csrf_token(self) -> str:
        response = self._client.get(f"{self.BASE_URL}/csrf")
        response.raise_for_status()
        token = response.headers.get("x-xsrf-token") or response.cookies.get(
            "XSRF-TOKEN"
        )
        if not token:
            raise ApiError("Failed to obtain CSRF token from Supernote")
        self._csrf_token = token
        return token

    def _api_call(self, endpoint: str, payload: dict[str, Any]) -> dict[str, Any]:
        if not self._csrf_token:
            self._fetch_csrf_token()

        headers = {
            "Content-Type": "application/json",
            "User-Agent": DEFAULT_USER_AGENT,
            "X-XSRF-TOKEN": self._csrf_token,
        }
        if self._access_token:
            headers["x-access-token"] = self._access_token

        url = f"{self.BASE_URL}{endpoint}"
        try:
            response = self._client.post(url, json=payload, headers=headers)
            response.raise_for_status()
        except httpx.HTTPStatusError as exc:
            if exc.response.status_code != 403:
                raise
            headers["X-XSRF-TOKEN"] = self._fetch_csrf_token()
            response = self._client.post(url, json=payload, headers=headers)
            response.raise_for_status()

        return response.json()
