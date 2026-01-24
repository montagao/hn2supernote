"""Tests for upload functionality."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest
from helpers import MockFileItem

from supernote_uploader import SessionError, SupernoteClient


class TestUpload:
    """Tests for file upload functionality."""

    def test_upload_pdf_success(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path
    ) -> None:
        """Test successful PDF upload."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        result = client.upload(temp_pdf, "/Inbox")

        assert result.success
        assert result.file_path == temp_pdf
        assert result.cloud_path == "/Inbox"
        assert result.file_name == "test.pdf"
        assert result.error is None
        patch_sn_client_class.put.assert_called_once()

    def test_upload_epub_success(
        self, patch_sn_client_class: MagicMock, temp_epub: Path
    ) -> None:
        """Test successful EPUB upload."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Documents", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        result = client.upload(temp_epub, "/Documents")

        assert result.success
        assert result.file_name == "test.epub"

    def test_upload_file_not_found(self, patch_sn_client_class: MagicMock) -> None:
        """Test upload with non-existent file."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")
        result = client.upload("/nonexistent/file.pdf", "/Inbox")

        assert not result.success
        assert "File not found" in (result.error or "")

    def test_upload_without_authentication_raises_error(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path
    ) -> None:
        """Test that upload without authentication raises SessionError."""
        patch_sn_client_class._access_token = None

        client = SupernoteClient(auto_login=False)

        with pytest.raises(SessionError, match="Not authenticated"):
            client.upload(temp_pdf, "/Inbox")

    def test_upload_creates_folder_when_missing(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path
    ) -> None:
        """Test that upload creates the target folder if missing."""
        patch_sn_client_class.login.return_value = "token"
        # First ls returns empty (folder doesn't exist), second returns the folder
        patch_sn_client_class.ls.side_effect = [
            [],  # Check for /Inbox
            [MockFileItem(id=1, file_name="NewFolder", is_folder=True)],  # After mkdir
        ]

        client = SupernoteClient("test@example.com", "password123")
        result = client.upload(temp_pdf, "/NewFolder")

        assert result.success
        patch_sn_client_class.mkdir.assert_called()

    def test_upload_normalizes_path_without_leading_slash(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path
    ) -> None:
        """Test that upload normalizes paths without leading slash."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        result = client.upload(temp_pdf, "Inbox")

        assert result.cloud_path == "/Inbox"

    def test_upload_failure_returns_result_with_error(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path
    ) -> None:
        """Test that upload failure returns result with error."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]
        patch_sn_client_class.put.side_effect = Exception("Network error")

        client = SupernoteClient("test@example.com", "password123")
        result = client.upload(temp_pdf, "/Inbox")

        assert not result.success
        assert "Network error" in (result.error or "")


class TestUploadMany:
    """Tests for batch upload functionality."""

    def test_upload_many_success(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path, temp_epub: Path
    ) -> None:
        """Test successful batch upload."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        results = client.upload_many([temp_pdf, temp_epub], "/Inbox")

        assert len(results) == 2
        assert all(r.success for r in results)

    def test_upload_many_partial_failure(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path, tmp_path: Path
    ) -> None:
        """Test batch upload with partial failure."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]
        # First upload succeeds, second fails
        patch_sn_client_class.put.side_effect = [None, Exception("Upload failed")]

        temp_pdf2 = tmp_path / "test2.pdf"
        temp_pdf2.write_bytes(b"%PDF-1.4 content")

        client = SupernoteClient("test@example.com", "password123")
        results = client.upload_many([temp_pdf, temp_pdf2], "/Inbox")

        assert len(results) == 2
        assert results[0].success
        assert not results[1].success

    def test_upload_many_stop_on_error(
        self, patch_sn_client_class: MagicMock, temp_pdf: Path, tmp_path: Path
    ) -> None:
        """Test batch upload stops on first error when stop_on_error=True."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]
        patch_sn_client_class.put.side_effect = Exception("Upload failed")

        temp_pdf2 = tmp_path / "test2.pdf"
        temp_pdf2.write_bytes(b"%PDF-1.4 content")

        client = SupernoteClient("test@example.com", "password123")
        results = client.upload_many(
            [temp_pdf, temp_pdf2], "/Inbox", stop_on_error=True
        )

        # Should stop after first failure
        assert len(results) == 1
        assert not results[0].success

    def test_upload_many_empty_list(self, patch_sn_client_class: MagicMock) -> None:
        """Test batch upload with empty list."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")
        results = client.upload_many([], "/Inbox")

        assert results == []
