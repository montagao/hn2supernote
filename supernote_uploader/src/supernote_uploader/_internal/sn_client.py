"""SNClient wrapper with CSRF token handling required by Supernote Cloud."""

from __future__ import annotations

import hashlib
import time

import httpx
from sncloud import SNClient, endpoints
from sncloud.api import calc_md5, calc_sha256
from sncloud.exceptions import ApiError
from sncloud.exceptions import AuthenticationError as SNAuthError

DEFAULT_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/107.0.0.0 Safari/537.36"
)


class SNClientWithCSRF(SNClient):
    """SNClient wrapper that adds CSRF token handling required by Supernote Cloud."""

    def __init__(self) -> None:
        super().__init__()
        self._client.headers.update({"User-Agent": DEFAULT_USER_AGENT})
        self._csrf_token: str | None = None
        self._last_login_timestamp: str | None = None
        self._last_auth_error_code: str | None = None
        self._last_auth_error_msg: str | None = None

    def _fetch_csrf_token(self) -> str:
        """Fetch CSRF token required by Supernote Cloud APIs.

        Stores token in self._csrf_token and relies on the shared client
        to hold the XSRF-TOKEN cookie.
        """
        resp = self._client.get(f"{self.BASE_URL}/csrf")
        resp.raise_for_status()
        token = resp.headers.get("x-xsrf-token") or resp.cookies.get("XSRF-TOKEN")
        if not token:
            raise ApiError("Failed to obtain CSRF token from Supernote")
        self._csrf_token = token
        return token

    def _ensure_csrf_token(self) -> None:
        """Ensure CSRF token is available, fetching if needed."""
        if not self._csrf_token:
            self._fetch_csrf_token()

    def _api_call(self, endpoint: str, payload: dict) -> dict:  # type: ignore[type-arg]
        """Make an API call with CSRF token handling."""
        headers: dict[str, str] = {
            "Content-Type": "application/json",
            "User-Agent": DEFAULT_USER_AGENT,
        }
        if self._access_token:
            headers["x-access-token"] = self._access_token

        # Supernote enforces CSRF protection on POST endpoints
        if endpoint != "/csrf":
            self._ensure_csrf_token()
            if self._csrf_token:
                headers["X-XSRF-TOKEN"] = self._csrf_token

        url = f"{self.BASE_URL}{endpoint}"
        try:
            response = self._client.post(url, json=payload, headers=headers)
            response.raise_for_status()
            return response.json()  # type: ignore[no-any-return]
        except httpx.HTTPStatusError as e:
            # If CSRF token expired or missing, refresh once and retry
            if e.response is not None and e.response.status_code == 403:
                self._fetch_csrf_token()
                if self._csrf_token:
                    headers["X-XSRF-TOKEN"] = self._csrf_token
                response = self._client.post(url, json=payload, headers=headers)
                response.raise_for_status()
                return response.json()  # type: ignore[no-any-return]
            raise

    def _extract_real_key(self, token: str) -> str:
        """Extract the real key from a pre-auth token."""
        if not token or "-" not in token:
            raise ApiError("Invalid pre-auth token format from Supernote")
        try:
            idx = int(token[-1])
        except ValueError as exc:
            raise ApiError("Invalid pre-auth token index from Supernote") from exc
        parts = token.split("-")
        if idx < 0 or idx >= len(parts):
            raise ApiError("Invalid pre-auth token index from Supernote")
        return parts[idx]

    def _hash256(self, text: str) -> str:
        """Compute SHA-256 hash of text."""
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def login(self, email: str, password: str) -> str:
        """Login to Supernote Cloud with email and password.

        Args:
            email: Account email address
            password: Account password

        Returns:
            Access token on success

        Raises:
            SNAuthError: If login fails
        """
        (rc, timestamp) = self._get_random_code(email)
        self._last_login_timestamp = str(timestamp)
        self._last_auth_error_code = None
        self._last_auth_error_msg = None

        pd = calc_sha256(calc_md5(password) + rc)
        payload = {
            "countryCode": 1,
            "account": email,
            "password": pd,
            "browser": "Chrome107",
            "equipment": "1",
            "loginMethod": "1",
            "timestamp": timestamp,
            "language": "en",
        }

        data = self._api_call(endpoints.login, payload)
        if not data.get("success"):
            self._last_auth_error_code = data.get("errorCode")
            self._last_auth_error_msg = data.get("errorMsg")
            raise SNAuthError(self._last_auth_error_msg or "Login failed")
        self._access_token = data["token"]
        return str(data["token"])

    def request_email_verification_code(
        self, email: str, timestamp: str | None = None
    ) -> dict[str, str]:
        """Request an email verification code for login.

        Args:
            email: Account email address
            timestamp: Optional timestamp (defaults to current time)

        Returns:
            Dict with email, timestamp, and valid_code_key for verification

        Raises:
            ApiError: If the request fails
        """
        ts = timestamp or str(int(time.time() * 1000))
        pre_auth = self._api_call("/user/validcode/pre-auth", {"account": email})
        if not pre_auth.get("success"):
            raise ApiError(pre_auth.get("errorMsg") or "Failed to pre-auth for verification")
        token = pre_auth.get("token")
        if not token:
            raise ApiError("Missing pre-auth token from Supernote")
        real_key = self._extract_real_key(token)
        sign = self._hash256(email + real_key)
        send_resp = self._api_call(
            "/user/mail/validcode/send",
            {"email": email, "timestamp": ts, "token": token, "sign": sign},
        )
        if not send_resp.get("success"):
            raise ApiError(send_resp.get("errorMsg") or "Failed to send verification code")
        valid_code_key = send_resp.get("validCodeKey")
        if not valid_code_key:
            raise ApiError("Missing validCodeKey from Supernote")
        return {"email": email, "timestamp": ts, "valid_code_key": str(valid_code_key)}

    def login_with_verification_code(
        self,
        email: str,
        verification_code: str,
        valid_code_key: str,
        timestamp: str,
    ) -> str:
        """Complete login using email verification code.

        Args:
            email: Account email address
            verification_code: The code received via email
            valid_code_key: The key from request_email_verification_code
            timestamp: The timestamp from request_email_verification_code

        Returns:
            Access token on success

        Raises:
            SNAuthError: If verification fails
        """
        payload = {
            "email": email,
            "validCode": verification_code,
            "validCodeKey": valid_code_key,
            "timestamp": timestamp,
            "browser": "Chrome107",
            "equipment": "4",
        }
        data = self._api_call("/official/user/sms/login", payload)
        if not data.get("success"):
            raise SNAuthError(data.get("errorMsg") or "Verification login failed")
        self._access_token = data["token"]
        return str(data["token"])
