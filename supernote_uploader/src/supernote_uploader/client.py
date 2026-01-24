"""Main SupernoteClient class for interacting with Supernote Cloud."""

from __future__ import annotations

import json
import logging
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from sncloud.exceptions import ApiError
from sncloud.exceptions import AuthenticationError as SNAuthError

from supernote_uploader._internal.sn_client import SNClientWithCSRF
from supernote_uploader.exceptions import (
    AuthenticationError,
    FolderError,
    SessionError,
    VerificationRequiredError,
)
from supernote_uploader.models import FileInfo, FolderInfo, UploadResult

logger = logging.getLogger(__name__)


class SupernoteClient:
    """Client for uploading files to Supernote Cloud.

    Supports both context manager and manual session patterns.

    Example (context manager - recommended):
        with SupernoteClient("user@example.com", "password") as client:
            client.upload("article.pdf", "/Inbox/Articles")

    Example (manual session):
        client = SupernoteClient()
        client.login("user@example.com", "password")
        client.upload("book.epub", "/Documents")
        client.close()
    """

    def __init__(
        self,
        email: str | None = None,
        password: str | None = None,
        *,
        auto_login: bool = True,
        token_cache_path: Path | str | None = None,
    ) -> None:
        """Initialize the client.

        Args:
            email: Account email address
            password: Account password
            auto_login: If True and credentials provided, login immediately
            token_cache_path: Optional path to cache access tokens
        """
        self._email = email
        self._password = password
        self._token_cache_path = Path(token_cache_path) if token_cache_path else None
        self._client: SNClientWithCSRF | None = None
        self._token_cache: dict[str, str] = {}
        self._token_cache_loaded = False

        if auto_login and email and password:
            self.login(email, password)

    def __enter__(self) -> SupernoteClient:
        """Enter context manager."""
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: Any,
    ) -> None:
        """Exit context manager."""
        self.close()

    @property
    def is_authenticated(self) -> bool:
        """Check if the client is authenticated."""
        return self._client is not None and self._client._access_token is not None

    def _ensure_authenticated(self) -> None:
        """Ensure the client is authenticated."""
        if not self.is_authenticated:
            raise SessionError("Not authenticated. Call login() first.")

    def _get_client(self) -> SNClientWithCSRF:
        """Get the underlying client, ensuring it exists."""
        if self._client is None:
            self._client = SNClientWithCSRF()
        return self._client

    def _load_token_cache(self) -> None:
        """Load token cache from disk."""
        if self._token_cache_loaded or not self._token_cache_path:
            return
        self._token_cache_loaded = True
        if not self._token_cache_path.exists():
            return
        try:
            data = json.loads(self._token_cache_path.read_text())
            if isinstance(data, dict):
                for email, value in data.items():
                    token = value.get("token") if isinstance(value, dict) else value
                    if email and token:
                        self._token_cache[email] = token
        except Exception as e:
            logger.warning(f"Failed to load token cache: {e}")

    def _save_token_cache(self) -> None:
        """Save token cache to disk."""
        if not self._token_cache_path:
            return
        try:
            payload = {
                email: {
                    "token": token,
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                }
                for email, token in self._token_cache.items()
            }
            self._token_cache_path.write_text(json.dumps(payload, indent=2))
        except Exception as e:
            logger.warning(f"Failed to save token cache: {e}")

    def _get_cached_token(self, email: str) -> str | None:
        """Get cached token for email."""
        self._load_token_cache()
        return self._token_cache.get(email)

    def _set_cached_token(self, email: str, token: str) -> None:
        """Cache token for email."""
        if email and token:
            self._token_cache[email] = token
            self._save_token_cache()

    def _clear_cached_token(self, email: str) -> None:
        """Clear cached token for email."""
        if email in self._token_cache:
            del self._token_cache[email]
            self._save_token_cache()

    def login(self, email: str | None = None, password: str | None = None) -> str:
        """Login to Supernote Cloud.

        Args:
            email: Account email (uses constructor value if not provided)
            password: Account password (uses constructor value if not provided)

        Returns:
            Access token on success

        Raises:
            AuthenticationError: If login fails
            VerificationRequiredError: If email verification is required
        """
        email = email or self._email
        password = password or self._password

        if not email or not password:
            raise AuthenticationError("Email and password are required")

        self._email = email
        self._password = password

        client = self._get_client()

        # Try cached token first
        cached_token = self._get_cached_token(email)
        if cached_token:
            client._access_token = cached_token
            try:
                client.ls(directory="/")
                logger.info("Using cached Supernote access token")
                return cached_token
            except Exception:
                logger.info("Cached token invalid; re-authenticating")
                self._clear_cached_token(email)
                client._access_token = None

        # Perform login
        try:
            token = client.login(email, password)
            self._set_cached_token(email, token)
            return token
        except SNAuthError as e:
            # Check for verification required error (E1760)
            if client._last_auth_error_code == "E1760":
                try:
                    verification = client.request_email_verification_code(
                        email, client._last_login_timestamp
                    )
                    raise VerificationRequiredError(
                        "Email verification required. A code was sent to your email.",
                        verification_context=verification,
                    )
                except ApiError as code_err:
                    raise AuthenticationError(
                        f"Verification required but sending code failed: {code_err}"
                    ) from code_err
            raise AuthenticationError(str(e)) from e

    def verify(self, code: str, context: dict[str, str]) -> str:
        """Complete login with email verification code.

        Args:
            code: The verification code received via email
            context: The verification_context from VerificationRequiredError

        Returns:
            Access token on success

        Raises:
            AuthenticationError: If verification fails
        """
        email = context.get("email")
        valid_code_key = context.get("valid_code_key")
        timestamp = context.get("timestamp")

        if not all([email, valid_code_key, timestamp]):
            raise AuthenticationError("Invalid verification context")

        client = self._get_client()
        try:
            token = client.login_with_verification_code(
                email=email,  # type: ignore[arg-type]
                verification_code=code,
                valid_code_key=valid_code_key,  # type: ignore[arg-type]
                timestamp=timestamp,  # type: ignore[arg-type]
            )
            self._set_cached_token(email, token)  # type: ignore[arg-type]
            return token
        except SNAuthError as e:
            raise AuthenticationError(f"Verification failed: {e}") from e

    def upload(
        self,
        file_path: str | Path,
        target_folder: str = "/Inbox",
        *,
        create_folder: bool = True,
    ) -> UploadResult:
        """Upload a file to Supernote Cloud.

        Args:
            file_path: Path to the file to upload (PDF or EPUB)
            target_folder: Cloud folder path to upload to
            create_folder: Create the target folder if it doesn't exist

        Returns:
            UploadResult with success status and details

        Raises:
            SessionError: If not authenticated
            UploadError: If upload fails
        """
        self._ensure_authenticated()
        client = self._get_client()

        file_path = Path(file_path)
        if not file_path.exists():
            return UploadResult(
                success=False,
                file_path=file_path,
                cloud_path=target_folder,
                file_name=file_path.name,
                error=f"File not found: {file_path}",
            )

        # Normalize target folder
        if not target_folder.startswith("/"):
            target_folder = "/" + target_folder

        # Ensure target folder exists
        if create_folder:
            try:
                if not self.folder_exists(target_folder):
                    self.mkdir(target_folder, parents=True)
            except FolderError as e:
                return UploadResult(
                    success=False,
                    file_path=file_path,
                    cloud_path=target_folder,
                    file_name=file_path.name,
                    error=f"Failed to create folder: {e}",
                )

        # Upload the file
        try:
            client.put(file_path=file_path, parent=target_folder)
            logger.info(f"Successfully uploaded {file_path.name} to {target_folder}")
            return UploadResult(
                success=True,
                file_path=file_path,
                cloud_path=target_folder,
                file_name=file_path.name,
            )
        except Exception as e:
            error_msg = f"Upload failed: {e}"
            logger.error(error_msg)
            return UploadResult(
                success=False,
                file_path=file_path,
                cloud_path=target_folder,
                file_name=file_path.name,
                error=error_msg,
            )

    def upload_many(
        self,
        file_paths: list[str | Path],
        target_folder: str = "/Inbox",
        *,
        create_folder: bool = True,
        stop_on_error: bool = False,
    ) -> list[UploadResult]:
        """Upload multiple files to Supernote Cloud.

        Args:
            file_paths: List of file paths to upload
            target_folder: Cloud folder path to upload to
            create_folder: Create the target folder if it doesn't exist
            stop_on_error: If True, stop uploading on first error

        Returns:
            List of UploadResult for each file
        """
        results: list[UploadResult] = []

        for file_path in file_paths:
            result = self.upload(
                file_path,
                target_folder,
                create_folder=create_folder,
            )
            results.append(result)

            if stop_on_error and not result.success:
                break

        return results

    def list_folder(self, folder_path: str = "/") -> list[FileInfo | FolderInfo]:
        """List contents of a folder in Supernote Cloud.

        Args:
            folder_path: Cloud folder path to list

        Returns:
            List of FileInfo and FolderInfo objects

        Raises:
            SessionError: If not authenticated
            FolderError: If listing fails
        """
        self._ensure_authenticated()
        client = self._get_client()

        if not folder_path.startswith("/"):
            folder_path = "/" + folder_path

        try:
            items = client.ls(directory=folder_path)
            result: list[FileInfo | FolderInfo] = []
            for item in items:
                if item.is_folder:
                    result.append(
                        FolderInfo(
                            id=item.id,
                            name=item.file_name,
                            path=f"{folder_path.rstrip('/')}/{item.file_name}",
                        )
                    )
                else:
                    result.append(
                        FileInfo(
                            id=item.id,
                            name=item.file_name,
                            path=f"{folder_path.rstrip('/')}/{item.file_name}",
                            size=item.file_size,
                        )
                    )
            return result
        except Exception as e:
            raise FolderError(f"Failed to list folder: {e}") from e

    def mkdir(self, folder_path: str, *, parents: bool = False) -> FolderInfo:
        """Create a folder in Supernote Cloud.

        Args:
            folder_path: Path of the folder to create
            parents: If True, create parent folders as needed

        Returns:
            FolderInfo for the created folder

        Raises:
            SessionError: If not authenticated
            FolderError: If creation fails
        """
        self._ensure_authenticated()
        client = self._get_client()

        if not folder_path.startswith("/"):
            folder_path = "/" + folder_path

        folder_path = folder_path.rstrip("/")
        if not folder_path:
            raise FolderError("Cannot create root folder")

        parent_path = os.path.dirname(folder_path)
        folder_name = os.path.basename(folder_path)

        if parents and parent_path and parent_path != "/" and not self.folder_exists(parent_path):
            # Recursively create parent folders
            self.mkdir(parent_path, parents=True)

        try:
            client.mkdir(folder_name, parent_path=parent_path or "/")
            logger.info(f"Created folder: {folder_path}")
            # Return a FolderInfo (we don't have the ID from mkdir response)
            return FolderInfo(
                id=0,  # ID unknown from mkdir response
                name=folder_name,
                path=folder_path,
            )
        except Exception as e:
            raise FolderError(f"Failed to create folder: {e}") from e

    def folder_exists(self, folder_path: str) -> bool:
        """Check if a folder exists in Supernote Cloud.

        Args:
            folder_path: Path of the folder to check

        Returns:
            True if the folder exists
        """
        self._ensure_authenticated()

        if not folder_path.startswith("/"):
            folder_path = "/" + folder_path

        folder_path = folder_path.rstrip("/")
        if not folder_path or folder_path == "/":
            return True  # Root always exists

        parent_path = os.path.dirname(folder_path)
        folder_name = os.path.basename(folder_path)

        try:
            items = self.list_folder(parent_path or "/")
            return any(
                isinstance(item, FolderInfo) and item.name == folder_name for item in items
            )
        except FolderError:
            return False

    def close(self) -> None:
        """Close the client and clean up resources."""
        self._client = None
