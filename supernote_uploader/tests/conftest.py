"""Pytest fixtures for supernote_uploader tests."""

from __future__ import annotations

from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest
from helpers import MockFileItem


@pytest.fixture
def mock_sn_client() -> MagicMock:
    """Create a mock SNClientWithCSRF."""
    client = MagicMock()
    client._access_token = "test_token"
    client._last_auth_error_code = None
    client._last_auth_error_msg = None
    client._last_login_timestamp = None
    return client


@pytest.fixture
def temp_pdf(tmp_path: Path) -> Path:
    """Create a temporary PDF file for testing."""
    pdf_path = tmp_path / "test.pdf"
    pdf_path.write_bytes(b"%PDF-1.4 test content")
    return pdf_path


@pytest.fixture
def temp_epub(tmp_path: Path) -> Path:
    """Create a temporary EPUB file for testing."""
    epub_path = tmp_path / "test.epub"
    epub_path.write_bytes(b"PK test epub content")
    return epub_path


@pytest.fixture
def mock_folder_items() -> list[MockFileItem]:
    """Create mock folder listing items."""
    return [
        MockFileItem(id=1, file_name="Documents", is_folder=True),
        MockFileItem(id=2, file_name="Inbox", is_folder=True),
        MockFileItem(id=3, file_name="test.pdf", is_folder=False, file_size=1024),
    ]


@pytest.fixture
def patch_sn_client_class() -> Any:
    """Patch SNClientWithCSRF class for testing."""
    with patch("supernote_uploader.client.SNClientWithCSRF") as mock_class:
        mock_instance = MagicMock()
        mock_instance._access_token = "test_token"
        mock_instance._last_auth_error_code = None
        mock_instance._last_auth_error_msg = None
        mock_instance._last_login_timestamp = None
        mock_class.return_value = mock_instance
        yield mock_instance
