import unittest

from sendtosupernote.app.sn_client import SNClientWithCSRF


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
    def __init__(self):
        self.headers = {}
        self.post_headers = None

    def get(self, url):
        self.csrf_url = url
        return FakeResponse(cookies={"XSRF-TOKEN": "csrf-token"})

    def post(self, url, *, json, headers):
        self.post_headers = headers
        return FakeResponse(payload={"success": True, "randomCode": "code"})


class SNClientWithCSRFTest(unittest.TestCase):
    def test_fetches_and_sends_csrf_token(self):
        client = SNClientWithCSRF()
        fake_http = FakeHttpClient()
        client._client = fake_http

        result = client._api_call("/login", {"account": "test@example.com"})

        self.assertTrue(result["success"])
        self.assertEqual(fake_http.post_headers["X-XSRF-TOKEN"], "csrf-token")
        self.assertEqual(fake_http.csrf_url, f"{client.BASE_URL}/csrf")


if __name__ == "__main__":
    unittest.main()
