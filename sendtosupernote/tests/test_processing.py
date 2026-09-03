import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from pydantic import ValidationError

from sendtosupernote.app import processing
from sendtosupernote.app.main import ArticleQueueRequest


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


class XPostProcessingTest(unittest.TestCase):
    def test_accepts_and_normalizes_x_post_metadata(self):
        request = ArticleQueueRequest(
            url="https://x.com/example/status/123",
            html_content="<article><p>Short post</p></article>",
            content_title="  Example on X  ",
            content_author="  @example  ",
            content_kind="x-post",
        )

        self.assertEqual(request.content_title, "Example on X")
        self.assertEqual(request.content_author, "@example")
        self.assertEqual(request.content_kind, "x-post")

    def test_rejects_unknown_content_kind(self):
        with self.assertRaises(ValidationError):
            ArticleQueueRequest(
                url="https://x.com/example/status/123",
                content_kind="social-post",
            )

    def test_accepts_short_extension_content_for_x_posts(self):
        result = processing.scrape_article_content(
            "https://x.com/example/status/123",
            raw_html_from_extension="<article><p>Short post</p></article>",
            content_kind="x-post",
        )

        self.assertIsNotNone(result)
        self.assertEqual(result["plain_text"], "Short post")

    def test_keeps_article_minimum_for_non_x_content(self):
        result = processing.scrape_article_content(
            "https://example.com/short",
            raw_html_from_extension="<article><p>Short post</p></article>",
            content_kind="article",
        )

        self.assertIsNone(result)

    def test_pdf_css_contains_x_post_media_constraints(self):
        css = processing.convert_markdown_to_styled_html(
            "",
            return_css_only=True,
        )

        self.assertIn(".x-post-media", css)
        self.assertIn("max-height: 70vh", css)


if __name__ == "__main__":
    unittest.main()
