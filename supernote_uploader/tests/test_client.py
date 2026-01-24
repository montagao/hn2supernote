"""Tests for SupernoteClient general functionality."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

from supernote_uploader import SupernoteClient


class TestClientInitialization:
    """Tests for client initialization."""

    def test_client_creates_without_credentials(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that client can be created without credentials."""
        client = SupernoteClient(auto_login=False)

        assert not client.is_authenticated

    def test_client_with_auto_login_false_does_not_login(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that auto_login=False prevents automatic login."""
        _client = SupernoteClient(
            "test@example.com", "password123", auto_login=False
        )
        assert _client is not None  # Use the variable

        patch_sn_client_class.login.assert_not_called()


class TestContextManager:
    """Tests for context manager functionality."""

    def test_context_manager_creates_authenticated_client(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that context manager creates authenticated client."""
        patch_sn_client_class.login.return_value = "token"

        with SupernoteClient("test@example.com", "password123") as client:
            assert client.is_authenticated

    def test_context_manager_closes_on_exit(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that context manager closes client on exit."""
        patch_sn_client_class.login.return_value = "token"

        with SupernoteClient("test@example.com", "password123") as client:
            pass

        # After exiting, the internal client should be None
        assert client._client is None

    def test_context_manager_closes_on_exception(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that context manager closes client on exception."""
        patch_sn_client_class.login.return_value = "token"

        try:
            with SupernoteClient("test@example.com", "password123") as client:
                raise ValueError("Test error")
        except ValueError:
            pass

        assert client._client is None


class TestClose:
    """Tests for close functionality."""

    def test_close_clears_client(self, patch_sn_client_class: MagicMock) -> None:
        """Test that close clears the internal client."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")
        assert client._client is not None

        client.close()
        assert client._client is None

    def test_close_can_be_called_multiple_times(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that close can be called multiple times safely."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")
        client.close()
        client.close()  # Should not raise


class TestIsAuthenticated:
    """Tests for is_authenticated property."""

    def test_is_authenticated_false_when_no_client(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that is_authenticated is False when no client exists."""
        client = SupernoteClient(auto_login=False)
        client._client = None

        assert not client.is_authenticated

    def test_is_authenticated_false_when_no_token(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that is_authenticated is False when no token."""
        patch_sn_client_class._access_token = None

        client = SupernoteClient(auto_login=False)

        assert not client.is_authenticated

    def test_is_authenticated_true_when_token_present(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that is_authenticated is True when token present."""
        patch_sn_client_class.login.return_value = "token"

        client = SupernoteClient("test@example.com", "password123")

        assert client.is_authenticated


class TestTokenCaching:
    """Tests for token caching functionality."""

    def test_token_is_cached_after_login(
        self, patch_sn_client_class: MagicMock, tmp_path: Path
    ) -> None:
        """Test that token is cached after successful login."""
        cache_path = tmp_path / "token_cache.json"
        patch_sn_client_class.login.return_value = "new_token"

        SupernoteClient(
            "test@example.com",
            "password123",
            token_cache_path=cache_path,
        )

        assert cache_path.exists()
        content = cache_path.read_text()
        assert "test@example.com" in content
        assert "new_token" in content

    def test_token_cache_includes_timestamp(
        self, patch_sn_client_class: MagicMock, tmp_path: Path
    ) -> None:
        """Test that token cache includes update timestamp."""
        import json

        cache_path = tmp_path / "token_cache.json"
        patch_sn_client_class.login.return_value = "token"

        SupernoteClient(
            "test@example.com",
            "password123",
            token_cache_path=cache_path,
        )

        data = json.loads(cache_path.read_text())
        assert "updated_at" in data["test@example.com"]
