import unittest

from sendtosupernote.app.sn_client import SNClientWithCSRF
from sncloud.exceptions import AuthenticationError


class FakeResponse:
    def __init__(self, *, headers=None, cookies=None, payload=None):
        self.headers = headers or {}
        self.cookies = cookies or {}
        self._payload = payload or {"success": True}
        self.status_code = 200

    def raise_for_status(self):
        return None

    def json(self):
        return self._payload


class FakeHttpClient:
    def __init__(self, post_payloads=None):
        self.headers = {}
        self.post_headers = None
        self.post_payloads = list(post_payloads or [])

    def get(self, url):
        self.csrf_url = url
        return FakeResponse(cookies={"XSRF-TOKEN": "csrf-token"})

    def post(self, url, *, json, headers):
        self.post_headers = headers
        payload = (
            self.post_payloads.pop(0)
            if self.post_payloads
            else {"success": True, "randomCode": "code"}
        )
        return FakeResponse(payload=payload)


class SNClientWithCSRFTest(unittest.TestCase):
    def test_fetches_and_sends_csrf_token(self):
        client = SNClientWithCSRF()
        fake_http = FakeHttpClient()
        client._client = fake_http

        result = client._api_call("/login", {"account": "test@example.com"})

        self.assertTrue(result["success"])
        self.assertEqual(fake_http.post_headers["X-XSRF-TOKEN"], "csrf-token")
        self.assertEqual(fake_http.csrf_url, f"{client.BASE_URL}/csrf")

    def test_captures_verification_required_login_response(self):
        client = SNClientWithCSRF()
        client._client = FakeHttpClient(
            [
                {"success": True, "randomCode": "random", "timestamp": "123"},
                {
                    "success": False,
                    "errorCode": "E1760",
                    "errorMsg": "Verification required",
                },
            ]
        )

        with self.assertRaises(AuthenticationError):
            client.login("reader@example.com", "password")

        self.assertEqual(client._last_auth_error_code, "E1760")
        self.assertEqual(client._last_login_timestamp, "123")

    def test_verification_code_login_returns_session_token(self):
        client = SNClientWithCSRF()
        client._client = FakeHttpClient(
            [{"success": True, "token": "supernote-session"}]
        )

        token = client.login_with_verification_code(
            email="reader@example.com",
            verification_code="123456",
            valid_code_key="verification-key",
            timestamp="123",
        )

        self.assertEqual(token, "supernote-session")
        self.assertEqual(client._access_token, "supernote-session")


if __name__ == "__main__":
    unittest.main()
