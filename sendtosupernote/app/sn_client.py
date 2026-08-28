"""Supernote Cloud client with the CSRF flow required by current APIs."""

from __future__ import annotations

import hashlib
import time
from typing import Any

import httpx
from sncloud import SNClient, endpoints
from sncloud.api import calc_md5, calc_sha256
from sncloud.exceptions import ApiError, AuthenticationError

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
        self._last_login_timestamp: str | None = None
        self._last_auth_error_code: str | None = None
        self._last_auth_error_msg: str | None = None

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

    def login(self, email: str, password: str) -> str:
        random_code, timestamp = self._get_random_code(email)
        self._last_login_timestamp = str(timestamp)
        self._last_auth_error_code = None
        self._last_auth_error_msg = None

        password_digest = calc_sha256(calc_md5(password) + random_code)
        data = self._api_call(
            endpoints.login,
            {
                "countryCode": 1,
                "account": email,
                "password": password_digest,
                "browser": "Chrome107",
                "equipment": "1",
                "loginMethod": "1",
                "timestamp": timestamp,
                "language": "en",
            },
        )
        if not data.get("success"):
            self._last_auth_error_code = data.get("errorCode")
            self._last_auth_error_msg = data.get("errorMsg")
            raise AuthenticationError(self._last_auth_error_msg or "Login failed")

        self._access_token = data["token"]
        return str(data["token"])

    @staticmethod
    def _extract_real_key(token: str) -> str:
        if not token or "-" not in token:
            raise ApiError("Invalid pre-auth token format from Supernote")
        try:
            index = int(token[-1])
        except ValueError as exc:
            raise ApiError("Invalid pre-auth token index from Supernote") from exc
        parts = token.split("-")
        if index < 0 or index >= len(parts):
            raise ApiError("Invalid pre-auth token index from Supernote")
        return parts[index]

    @staticmethod
    def _hash256(text: str) -> str:
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def request_email_verification_code(
        self, email: str, timestamp: str | None = None
    ) -> dict[str, str]:
        request_timestamp = timestamp or str(int(time.time() * 1000))
        pre_auth = self._api_call("/user/validcode/pre-auth", {"account": email})
        if not pre_auth.get("success"):
            raise ApiError(pre_auth.get("errorMsg") or "Verification pre-auth failed")

        pre_auth_token = pre_auth.get("token")
        if not pre_auth_token:
            raise ApiError("Missing pre-auth token from Supernote")
        real_key = self._extract_real_key(pre_auth_token)
        signature = self._hash256(email + real_key)

        send_response = self._api_call(
            "/user/mail/validcode/send",
            {
                "email": email,
                "timestamp": request_timestamp,
                "token": pre_auth_token,
                "sign": signature,
            },
        )
        if not send_response.get("success"):
            raise ApiError(send_response.get("errorMsg") or "Sending code failed")
        valid_code_key = send_response.get("validCodeKey")
        if not valid_code_key:
            raise ApiError("Missing verification key from Supernote")
        return {
            "email": email,
            "timestamp": request_timestamp,
            "valid_code_key": str(valid_code_key),
        }

    def login_with_verification_code(
        self,
        email: str,
        verification_code: str,
        valid_code_key: str,
        timestamp: str,
    ) -> str:
        data = self._api_call(
            "/official/user/sms/login",
            {
                "email": email,
                "validCode": verification_code,
                "validCodeKey": valid_code_key,
                "timestamp": timestamp,
                "browser": "Chrome107",
                "equipment": "4",
            },
        )
        if not data.get("success"):
            raise AuthenticationError(data.get("errorMsg") or "Verification failed")
        self._access_token = data["token"]
        return str(data["token"])
