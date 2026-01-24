"""Tests for authentication functionality."""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

import pytest
from sncloud.exceptions import AuthenticationError as SNAuthError

from supernote_uploader import AuthenticationError, SupernoteClient, VerificationRequiredError


class TestLogin:
    """Tests for login functionality."""

    def test_login_with_constructor_credentials(self, patch_sn_client_class: MagicMock) -> None:
        """Test login with credentials provided to constructor."""
        patch_sn_client_class.login.return_value = "access_token"
        patch_sn_client_class.ls.return_value = []

        client = SupernoteClient("test@example.com", "password123")

        assert client.is_authenticated
        patch_sn_client_class.login.assert_called_once_with("test@example.com", "password123")

    def test_login_with_method_credentials(self, patch_sn_client_class: MagicMock) -> None:
        """Test login with credentials provided to login method."""
        patch_sn_client_class.login.return_value = "access_token"

        client = SupernoteClient(auto_login=False)
        token = client.login("test@example.com", "password123")

        assert token == "access_token"
        assert client.is_authenticated

    def test_login_without_credentials_raises_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that login without credentials raises AuthenticationError."""
        client = SupernoteClient(auto_login=False)

        with pytest.raises(AuthenticationError, match="Email and password are required"):
            client.login()

    def test_login_failure_raises_authentication_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that login failure raises AuthenticationError."""
        patch_sn_client_class.login.side_effect = SNAuthError("Invalid credentials")

        with pytest.raises(AuthenticationError, match="Invalid credentials"):
            SupernoteClient("test@example.com", "wrong_password")

    def test_login_uses_cached_token_when_valid(
        self, patch_sn_client_class: MagicMock, tmp_path: Any
    ) -> None:
        """Test that cached token is used when valid."""
        cache_path = tmp_path / "token_cache.json"
        cache_path.write_text('{"test@example.com": {"token": "cached_token"}}')

        patch_sn_client_class.ls.return_value = []

        client = SupernoteClient(
            "test@example.com",
            "password123",
            token_cache_path=cache_path,
        )

        assert client.is_authenticated
        patch_sn_client_class.login.assert_not_called()

    def test_login_clears_invalid_cached_token(
        self, patch_sn_client_class: MagicMock, tmp_path: Any
    ) -> None:
        """Test that invalid cached token is cleared and re-login occurs."""
        cache_path = tmp_path / "token_cache.json"
        cache_path.write_text('{"test@example.com": {"token": "invalid_token"}}')

        patch_sn_client_class.ls.side_effect = Exception("Token expired")

        # Simulate login setting _access_token (as the real SNClientWithCSRF does)
        def mock_login(email: str, password: str) -> str:
            patch_sn_client_class._access_token = "new_token"
            return "new_token"

        patch_sn_client_class.login.side_effect = mock_login

        client = SupernoteClient(
            "test@example.com",
            "password123",
            token_cache_path=cache_path,
        )

        assert client.is_authenticated
        patch_sn_client_class.login.assert_called_once()


class TestVerification:
    """Tests for email verification flow."""

    def test_verification_required_raises_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that verification required error is raised with context."""
        patch_sn_client_class._last_auth_error_code = "E1760"
        patch_sn_client_class.login.side_effect = SNAuthError("Verification required")
        patch_sn_client_class.request_email_verification_code.return_value = {
            "email": "test@example.com",
            "timestamp": "1234567890",
            "valid_code_key": "key123",
        }

        with pytest.raises(VerificationRequiredError) as exc_info:
            SupernoteClient("test@example.com", "password123")

        assert exc_info.value.verification_context["email"] == "test@example.com"
        assert "valid_code_key" in exc_info.value.verification_context

    def test_verify_completes_login(self, patch_sn_client_class: MagicMock) -> None:
        """Test that verify() completes login with verification code."""
        patch_sn_client_class.login_with_verification_code.return_value = "access_token"

        client = SupernoteClient(auto_login=False)
        context = {
            "email": "test@example.com",
            "timestamp": "1234567890",
            "valid_code_key": "key123",
        }

        token = client.verify("123456", context)

        assert token == "access_token"
        patch_sn_client_class.login_with_verification_code.assert_called_once_with(
            email="test@example.com",
            verification_code="123456",
            valid_code_key="key123",
            timestamp="1234567890",
        )

    def test_verify_with_invalid_context_raises_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that verify with incomplete context raises error."""
        client = SupernoteClient(auto_login=False)

        with pytest.raises(AuthenticationError, match="Invalid verification context"):
            client.verify("123456", {"email": "test@example.com"})

    def test_verify_failure_raises_authentication_error(
        self, patch_sn_client_class: MagicMock
    ) -> None:
        """Test that verification failure raises AuthenticationError."""
        patch_sn_client_class.login_with_verification_code.side_effect = SNAuthError(
            "Invalid code"
        )

        client = SupernoteClient(auto_login=False)
        context = {
            "email": "test@example.com",
            "timestamp": "1234567890",
            "valid_code_key": "key123",
        }

        with pytest.raises(AuthenticationError, match="Verification failed"):
            client.verify("wrong_code", context)
