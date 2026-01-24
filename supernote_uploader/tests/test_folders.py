"""Tests for folder operations."""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest
from helpers import MockFileItem

from supernote_uploader import FileInfo, FolderError, FolderInfo, SessionError, SupernoteClient


class TestListFolder:
    """Tests for folder listing functionality."""

    def test_list_folder_returns_files_and_folders(
        self, patch_sn_client_class: MagicMock, mock_folder_items: list[MockFileItem]
    ) -> None:
        """Test that list_folder returns FileInfo and FolderInfo objects."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = mock_folder_items

        client = SupernoteClient("test@example.com", "password123")
        items = client.list_folder("/")

        assert len(items) == 3

        folders = [i for i in items if isinstance(i, FolderInfo)]
        files = [i for i in items if isinstance(i, FileInfo)]

        assert len(folders) == 2
        assert len(files) == 1

        assert folders[0].name == "Documents"
        assert files[0].name == "test.pdf"
        assert files[0].size == 1024

    def test_list_folder_normalizes_path(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that list_folder normalizes paths without leading slash."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = []

        client = SupernoteClient("test@example.com", "password123")
        client.list_folder("Documents")

        patch_sn_client_class.ls.assert_called_with(directory="/Documents")

    def test_list_folder_without_authentication_raises_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that list_folder without authentication raises SessionError."""
        patch_sn_client_class._access_token = None

        client = SupernoteClient(auto_login=False)

        with pytest.raises(SessionError, match="Not authenticated"):
            client.list_folder("/")

    def test_list_folder_error_raises_folder_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that listing error raises FolderError."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.side_effect = Exception("Network error")

        client = SupernoteClient("test@example.com", "password123")

        with pytest.raises(FolderError, match="Failed to list folder"):
            client.list_folder("/")


class TestMkdir:
    """Tests for folder creation functionality."""

    def test_mkdir_creates_folder(self, patch_sn_client_class: MagicMock) -> None:
        """Test that mkdir creates a folder."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = []

        client = SupernoteClient("test@example.com", "password123")
        result = client.mkdir("/NewFolder")

        assert isinstance(result, FolderInfo)
        assert result.name == "NewFolder"
        assert result.path == "/NewFolder"
        patch_sn_client_class.mkdir.assert_called_once_with("NewFolder", parent_path="/")

    def test_mkdir_with_parents(self, patch_sn_client_class: MagicMock) -> None:
        """Test that mkdir with parents=True creates parent folders."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = []

        client = SupernoteClient("test@example.com", "password123")
        client.mkdir("/Parent/Child/Grandchild", parents=True)

        # Should create parent folders
        assert patch_sn_client_class.mkdir.call_count >= 1

    def test_mkdir_normalizes_path(self, patch_sn_client_class: MagicMock) -> None:
        """Test that mkdir normalizes paths without leading slash."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Parent", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        client.mkdir("Parent/Child")

        patch_sn_client_class.mkdir.assert_called_with("Child", parent_path="/Parent")

    def test_mkdir_root_folder_raises_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that mkdir for root folder raises FolderError."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")

        with pytest.raises(FolderError, match="Cannot create root folder"):
            client.mkdir("/")

    def test_mkdir_error_raises_folder_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that mkdir error raises FolderError."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = []
        patch_sn_client_class.mkdir.side_effect = Exception("Permission denied")

        client = SupernoteClient("test@example.com", "password123")

        with pytest.raises(FolderError, match="Failed to create folder"):
            client.mkdir("/ProtectedFolder")


class TestFolderExists:
    """Tests for folder existence check."""

    def test_folder_exists_returns_true_when_folder_exists(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that folder_exists returns True when folder exists."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Inbox", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        exists = client.folder_exists("/Inbox")

        assert exists is True

    def test_folder_exists_returns_false_when_folder_missing(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that folder_exists returns False when folder is missing."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="Documents", is_folder=True)
        ]

        client = SupernoteClient("test@example.com", "password123")
        exists = client.folder_exists("/Missing")

        assert exists is False

    def test_folder_exists_returns_false_for_file(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that folder_exists returns False for a file."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.return_value = [
            MockFileItem(id=1, file_name="test.pdf", is_folder=False)
        ]

        client = SupernoteClient("test@example.com", "password123")
        exists = client.folder_exists("/test.pdf")

        assert exists is False

    def test_folder_exists_root_always_exists(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that folder_exists returns True for root."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")
        exists = client.folder_exists("/")

        assert exists is True

    def test_folder_exists_returns_false_on_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that folder_exists returns False on error."""
        patch_sn_client_class.login.return_value = "token"
        patch_sn_client_class.ls.side_effect = Exception("Network error")

        client = SupernoteClient("test@example.com", "password123")
        exists = client.folder_exists("/SomeFolder")

        assert exists is False
