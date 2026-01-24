"""Tests for CLI functionality."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from click.testing import CliRunner

from supernote_uploader.cli import main
from supernote_uploader.exceptions import AuthenticationError, VerificationRequiredError
from supernote_uploader.models import FileInfo, FolderInfo, UploadResult


@pytest.fixture
def runner() -> CliRunner:
    """Create a CLI test runner."""
    return CliRunner()


@pytest.fixture
def mock_client() -> MagicMock:
    """Create a mock SupernoteClient."""
    client = MagicMock()
    client.is_authenticated = True
    client._access_token = "test_token"
    return client


class TestLoginCommand:
    """Tests for the login command."""

    def test_login_success(self, runner: CliRunner) -> None:
        """Test successful login."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_get_client.return_value = mock_client

            result = runner.invoke(
                main,
                ["login", "--email", "test@example.com", "--password", "password123"],
            )

            assert result.exit_code == 0
            assert "Login successful" in result.output
            mock_client.login.assert_called_once()

    def test_login_with_verification(self, runner: CliRunner) -> None:
        """Test login with email verification."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_get_client.return_value = mock_client
            mock_client.login.side_effect = VerificationRequiredError(
                "Verification required",
                verification_context={
                    "email": "test@example.com",
                    "timestamp": "123",
                    "valid_code_key": "key",
                },
            )

            result = runner.invoke(
                main,
                ["login", "--email", "test@example.com", "--password", "password123"],
                input="123456\n",
            )

            assert result.exit_code == 0
            assert "Verification successful" in result.output
            mock_client.verify.assert_called_once()

    def test_login_failure(self, runner: CliRunner) -> None:
        """Test login failure."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_get_client.return_value = mock_client
            mock_client.login.side_effect = AuthenticationError("Invalid credentials")

            result = runner.invoke(
                main,
                ["login", "--email", "test@example.com", "--password", "wrong"],
            )

            assert result.exit_code == 1
            assert "Login failed" in result.output


class TestUploadCommand:
    """Tests for the upload command."""

    def test_upload_single_file(self, runner: CliRunner, tmp_path: Path) -> None:
        """Test uploading a single file."""
        test_file = tmp_path / "test.pdf"
        test_file.write_bytes(b"%PDF-1.4 content")

        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.upload_many.return_value = [
                UploadResult(
                    success=True,
                    file_path=test_file,
                    cloud_path="/Inbox",
                    file_name="test.pdf",
                )
            ]

            result = runner.invoke(main, ["upload", str(test_file)])

            assert result.exit_code == 0
            assert "test.pdf" in result.output
            assert "uploaded successfully" in result.output

    def test_upload_multiple_files(self, runner: CliRunner, tmp_path: Path) -> None:
        """Test uploading multiple files."""
        test_files = []
        for i in range(3):
            f = tmp_path / f"test{i}.pdf"
            f.write_bytes(b"%PDF-1.4 content")
            test_files.append(f)

        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.upload_many.return_value = [
                UploadResult(
                    success=True,
                    file_path=f,
                    cloud_path="/Inbox",
                    file_name=f.name,
                )
                for f in test_files
            ]

            result = runner.invoke(main, ["upload"] + [str(f) for f in test_files])

            assert result.exit_code == 0
            assert "3 file(s) uploaded successfully" in result.output

    def test_upload_with_custom_folder(self, runner: CliRunner, tmp_path: Path) -> None:
        """Test uploading to a custom folder."""
        test_file = tmp_path / "test.pdf"
        test_file.write_bytes(b"%PDF-1.4 content")

        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.upload_many.return_value = [
                UploadResult(
                    success=True,
                    file_path=test_file,
                    cloud_path="/Documents/Articles",
                    file_name="test.pdf",
                )
            ]

            result = runner.invoke(
                main, ["upload", str(test_file), "--folder", "/Documents/Articles"]
            )

            assert result.exit_code == 0
            mock_client.upload_many.assert_called_once()
            call_args = mock_client.upload_many.call_args
            assert call_args[0][1] == "/Documents/Articles"

    def test_upload_partial_failure(self, runner: CliRunner, tmp_path: Path) -> None:
        """Test upload with partial failure."""
        test_files = [tmp_path / f"test{i}.pdf" for i in range(2)]
        for f in test_files:
            f.write_bytes(b"%PDF-1.4 content")

        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.upload_many.return_value = [
                UploadResult(
                    success=True,
                    file_path=test_files[0],
                    cloud_path="/Inbox",
                    file_name="test0.pdf",
                ),
                UploadResult(
                    success=False,
                    file_path=test_files[1],
                    cloud_path="/Inbox",
                    file_name="test1.pdf",
                    error="Network error",
                ),
            ]

            result = runner.invoke(main, ["upload"] + [str(f) for f in test_files])

            assert result.exit_code == 1
            assert "1/2 file(s) uploaded" in result.output


class TestLsCommand:
    """Tests for the ls command."""

    def test_ls_root(self, runner: CliRunner) -> None:
        """Test listing root folder."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.list_folder.return_value = [
                FolderInfo(id=1, name="Documents", path="/Documents"),
                FolderInfo(id=2, name="Inbox", path="/Inbox"),
                FileInfo(id=3, name="readme.pdf", path="/readme.pdf", size=1024),
            ]

            result = runner.invoke(main, ["ls"])

            assert result.exit_code == 0
            assert "Documents/" in result.output
            assert "Inbox/" in result.output
            assert "readme.pdf" in result.output

    def test_ls_specific_folder(self, runner: CliRunner) -> None:
        """Test listing a specific folder."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.list_folder.return_value = [
                FileInfo(id=1, name="article.pdf", path="/Inbox/article.pdf", size=2048),
            ]

            result = runner.invoke(main, ["ls", "/Inbox"])

            assert result.exit_code == 0
            assert "article.pdf" in result.output
            mock_client.list_folder.assert_called_with("/Inbox")

    def test_ls_empty_folder(self, runner: CliRunner) -> None:
        """Test listing an empty folder."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.list_folder.return_value = []

            result = runner.invoke(main, ["ls", "/Empty"])

            assert result.exit_code == 0
            assert "empty folder" in result.output


class TestMkdirCommand:
    """Tests for the mkdir command."""

    def test_mkdir_success(self, runner: CliRunner) -> None:
        """Test creating a folder."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client

            result = runner.invoke(main, ["mkdir", "/Inbox/Articles"])

            assert result.exit_code == 0
            assert "Created folder" in result.output
            mock_client.mkdir.assert_called_with("/Inbox/Articles", parents=False)

    def test_mkdir_with_parents(self, runner: CliRunner) -> None:
        """Test creating a folder with parent directories."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client

            result = runner.invoke(main, ["mkdir", "/A/B/C", "--parents"])

            assert result.exit_code == 0
            mock_client.mkdir.assert_called_with("/A/B/C", parents=True)

    def test_mkdir_failure(self, runner: CliRunner) -> None:
        """Test mkdir failure."""
        with patch("supernote_uploader.cli.get_client") as mock_get_client:
            mock_client = MagicMock()
            mock_client.is_authenticated = True
            mock_get_client.return_value = mock_client
            mock_client.mkdir.side_effect = Exception("Folder already exists")

            result = runner.invoke(main, ["mkdir", "/Existing"])

            assert result.exit_code == 1
            assert "Error" in result.output


class TestFormatSize:
    """Tests for the _format_size helper."""

    def test_format_bytes(self) -> None:
        """Test formatting bytes."""
        from supernote_uploader.cli import _format_size

        assert _format_size(0) == "0 B"
        assert _format_size(512) == "512 B"

    def test_format_kilobytes(self) -> None:
        """Test formatting kilobytes."""
        from supernote_uploader.cli import _format_size

        assert _format_size(1024) == "1.0 KB"
        assert _format_size(2048) == "2.0 KB"

    def test_format_megabytes(self) -> None:
        """Test formatting megabytes."""
        from supernote_uploader.cli import _format_size

        assert _format_size(1024 * 1024) == "1.0 MB"
        assert _format_size(5 * 1024 * 1024) == "5.0 MB"

    def test_format_gigabytes(self) -> None:
        """Test formatting gigabytes."""
        from supernote_uploader.cli import _format_size

        assert _format_size(1024 * 1024 * 1024) == "1.0 GB"
