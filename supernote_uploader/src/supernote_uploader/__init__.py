"""Supernote Uploader - A Python library for uploading files to Supernote Cloud.

Example usage:
    from supernote_uploader import SupernoteClient

    # Using context manager (recommended)
    with SupernoteClient("user@example.com", "password") as client:
        result = client.upload("article.pdf", "/Inbox/Articles")
        print(f"Upload {'succeeded' if result.success else 'failed'}")

    # Manual session management
    client = SupernoteClient()
    client.login("user@example.com", "password")
    client.upload("book.epub", "/Documents")
    client.close()
"""

from supernote_uploader.client import SupernoteClient
from supernote_uploader.exceptions import (
    AuthenticationError,
    FolderError,
    SessionError,
    SupernoteError,
    UploadError,
    VerificationRequiredError,
)
from supernote_uploader.models import FileInfo, FolderInfo, UploadResult

__version__ = "0.1.0"

__all__ = [
    # Main client
    "SupernoteClient",
    # Models
    "FileInfo",
    "FolderInfo",
    "UploadResult",
    # Exceptions
    "SupernoteError",
    "AuthenticationError",
    "VerificationRequiredError",
    "UploadError",
    "FolderError",
    "SessionError",
]
