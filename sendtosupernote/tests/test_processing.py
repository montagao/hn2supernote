import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from sendtosupernote.app import processing


class UploadAuthenticationTest(unittest.TestCase):
    @patch("sendtosupernote.app.processing.SNClientWithCSRF")
    def test_reuses_access_token_without_password_login(self, client_class):
        client = client_class.return_value
        client.ls.return_value = [
            SimpleNamespace(file_name="SendToSupernote", is_folder=True)
        ]

        with tempfile.NamedTemporaryFile(suffix=".pdf") as pdf_file:
            uploaded = processing.upload_pdfs_to_supernote(
                [pdf_file.name],
                sn_email="reader@example.com",
                sn_access_token="cloud-session-token",
            )

        self.assertEqual(uploaded, 1)
        self.assertEqual(client._access_token, "cloud-session-token")
        client.login.assert_not_called()
        client.put.assert_called_once_with(
            file_path=Path(pdf_file.name),
            parent="/Inbox/SendToSupernote",
        )


if __name__ == "__main__":
    unittest.main()
