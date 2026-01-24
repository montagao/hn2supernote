"""Exception hierarchy for the supernote_uploader library."""

from __future__ import annotations

from typing import Any


class SupernoteError(Exception):
    """Base exception for all supernote_uploader errors."""

    pass


class AuthenticationError(SupernoteError):
    """Raised when authentication fails."""

    pass


class VerificationRequiredError(AuthenticationError):
    """Raised when email verification is required to complete login.

    The verification_context attribute contains the data needed to complete
    verification via the verify() method.
    """

    def __init__(self, message: str, verification_context: dict[str, Any]) -> None:
        super().__init__(message)
        self.verification_context = verification_context


class UploadError(SupernoteError):
    """Raised when a file upload fails."""

    pass


class FolderError(SupernoteError):
    """Raised when a folder operation fails."""

    pass


class SessionError(SupernoteError):
    """Raised when there's an issue with the session state."""

    pass
