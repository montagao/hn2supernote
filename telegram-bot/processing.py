"""
Content processing pipeline for the Telegram bot.
Handles scraping, content extraction, Markdown formatting, PDF generation, and Supernote upload.
"""

import logging
import traceback
import json
import re
import os
import tempfile
from pathlib import Path
from urllib.parse import urljoin

from trafilatura import extract as trafilatura_extract
from playwright.sync_api import sync_playwright, TimeoutError as PlaywrightTimeoutError
import google.generativeai as genai
import markdown2
from bs4 import BeautifulSoup
from sncloud import SNClient

# Configure logger
logger = logging.getLogger(__name__)

# Constants
MIN_CONTENT_LENGTH = 150
HTML_SNIPPET_LENGTH = 1000


def scrape_article_content(url: str) -> dict | None:
    """
    Scrape article content from a URL using Playwright and Trafilatura.

    Returns a dictionary with:
        - title: Article title
        - html_content: Cleaned HTML for PDF fallback
        - plain_text: Extracted plain text
        - extracted_date: Publication date if found
        - author: Author name if found
        - image_urls: List of image URLs found in the article

    Returns None if scraping fails or content is insufficient.
    """
    logger.info(f"Starting content extraction for {url}")

    title = "Untitled Article"
    plain_text = None
    cleaned_html_content = None
    extracted_date = None
    author = None
    image_urls = []
    html_source_to_process = None

    # Fetch page with Playwright
    try:
        with sync_playwright() as p:
            browser = p.firefox.launch(headless=True, timeout=60000)
            page = browser.new_page()
            page.set_default_navigation_timeout(45000)
            page.set_default_timeout(30000)
            logger.info(f"Playwright: Navigating to {url}")
            page.goto(url, wait_until="networkidle", timeout=45000)
            logger.info(f"Playwright: Page loaded. Extracting content for {url}")
            html_source_to_process = page.content()
            logger.info(f"Playwright: Fetched HTML. Length: {len(html_source_to_process)}")
            browser.close()
    except PlaywrightTimeoutError as pte:
        logger.error(f"FAIL {url}: Playwright timeout: {pte}")
        return None
    except Exception as e_pw:
        logger.error(f"FAIL {url}: Playwright failed: {e_pw}\n{traceback.format_exc()}")
        return None

    if not html_source_to_process:
        logger.error(f"FAIL {url}: HTML source is empty after Playwright fetch.")
        return None

    # Trafilatura extraction - JSON format for metadata
    try:
        metadata_settings = {
            'output_format': 'json',
            'with_metadata': True,
            'include_comments': False,
            'include_tables': True,
            'include_formatting': True,
            'favor_recall': True
        }
        logger.info(f"Trafilatura (JSON): Extracting for {url}")
        json_string_data = trafilatura_extract(html_source_to_process, **metadata_settings)

        if json_string_data:
            try:
                extracted_data = json.loads(json_string_data)
                if extracted_data and isinstance(extracted_data, dict):
                    plain_text = extracted_data.get('text')
                    title = extracted_data.get('title') or title
                    extracted_date = extracted_data.get('date')
                    author = extracted_data.get('author')
                    logger.info(f"Trafilatura (JSON): title='{title}', text_len={len(plain_text) if plain_text else 0}")
            except json.JSONDecodeError as e_json:
                logger.warning(f"Trafilatura (JSON): Failed to parse: {e_json}")
    except Exception as e_extract:
        logger.error(f"Error during Trafilatura JSON extraction: {e_extract}")

    # Text fallback if JSON extraction was insufficient
    plain_text_len = len(plain_text) if plain_text else 0
    if not plain_text or plain_text_len < MIN_CONTENT_LENGTH:
        logger.info(f"Trafilatura (Text Fallback): Triggering for {url}")
        try:
            text_settings = {
                'output_format': 'txt',
                'include_comments': False,
                'include_tables': True,
                'favor_recall': True
            }
            plain_text_fallback = trafilatura_extract(html_source_to_process, **text_settings)
            if plain_text_fallback and len(plain_text_fallback) >= MIN_CONTENT_LENGTH:
                plain_text = plain_text_fallback
                logger.info(f"Trafilatura (Text Fallback): Success. Length: {len(plain_text)}")
        except Exception as e_txt:
            logger.error(f"Error during text fallback: {e_txt}")

    # Title fallback using BeautifulSoup
    if not title or title == "Untitled Article" or not title.strip():
        try:
            soup = BeautifulSoup(html_source_to_process, 'html.parser')
            if soup.title and soup.title.string:
                title = soup.title.string.strip() or "Untitled Article"
                logger.info(f"BeautifulSoup: Extracted title: '{title}'")
        except Exception as e_bs:
            logger.error(f"Error during BeautifulSoup title extraction: {e_bs}")

    if not title or not title.strip():
        title = "Untitled Article"

    # Check content sufficiency
    if not plain_text or len(plain_text) < MIN_CONTENT_LENGTH:
        logger.error(f"FAIL {url}: Insufficient content (length: {len(plain_text) if plain_text else 0})")
        return None

    # Extract cleaned HTML for PDF fallback
    try:
        html_settings = {
            'output_format': 'html',
            'include_comments': False,
            'include_tables': True,
            'include_formatting': True,
            'favor_recall': True
        }
        cleaned_html_content = trafilatura_extract(html_source_to_process, **html_settings)
        if not cleaned_html_content:
            logger.warning(f"Trafilatura (HTML): No cleaned HTML. Constructing from plain text.")
            escaped_title = title.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
            escaped_text = plain_text.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;').replace('\n', '<br />\n')
            cleaned_html_content = f"<!DOCTYPE html><html><head><title>{escaped_title}</title></head><body><h1>{escaped_title}</h1><div>{escaped_text}</div></body></html>"
    except Exception as e_html:
        logger.error(f"Error during HTML extraction: {e_html}")
        escaped_title = title.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
        escaped_text = plain_text.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;').replace('\n', '<br />\n')
        cleaned_html_content = f"<!DOCTYPE html><html><head><title>{escaped_title}</title></head><body><h1>{escaped_title}</h1><div>{escaped_text}</div></body></html>"

    # Extract image URLs
    if cleaned_html_content:
        try:
            soup = BeautifulSoup(cleaned_html_content, 'html.parser')
            for img in soup.find_all('img'):
                src = img.get('src')
                if src:
                    absolute_src = urljoin(url, src)
                    if absolute_src not in image_urls:
                        image_urls.append(absolute_src)
            if image_urls:
                logger.info(f"Extracted {len(image_urls)} image URLs")
        except Exception as e_img:
            logger.warning(f"Could not extract images: {e_img}")

    logger.info(f"Successfully processed {url}. Title: '{title}', text_len: {len(plain_text)}")
    return {
        'title': title,
        'html_content': cleaned_html_content,
        'plain_text': plain_text,
        'extracted_date': extracted_date,
        'author': author,
        'image_urls': image_urls
    }


def classify_article_quality(article_text: str, api_key: str) -> bool:
    """
    Classify article quality using Gemini API.
    Returns True if thought-provoking, False if advertisement/low-quality.
    Defaults to True on errors.
    """
    if not api_key:
        logger.warning("No Gemini API key. Skipping classification, defaulting to good.")
        return True

    try:
        genai.configure(api_key=api_key)
        model = genai.GenerativeModel('gemini-2.0-flash')
        prompt = (
            "You are an expert content quality analyst. Classify this article: "
            "Is it substantive and thought-provoking, or is it primarily an advertisement/promotional/superficial? "
            "Respond with only one word: 'thought-provoking' or 'advertisement'.\n\n"
            f"Article (first 10000 chars):\n{article_text[:10000]}"
        )

        response = model.generate_content(prompt)

        if not response.candidates:
            logger.warning(f"Gemini (classify): No candidates. Feedback: {response.prompt_feedback}")
            return True

        candidate = response.candidates[0]
        if not candidate.content or not candidate.content.parts:
            logger.warning(f"Gemini (classify): No content. Finish: {candidate.finish_reason}")
            return True

        response_text = candidate.content.parts[0].text.strip().lower()
        logger.info(f"Gemini classification: '{response_text}'")

        if "thought-provoking" in response_text:
            return True
        elif "advertisement" in response_text:
            return False
        else:
            logger.warning(f"Gemini classification unclear: '{response_text}'. Defaulting to good.")
            return True

    except Exception as e:
        logger.error(f"Error during Gemini classification: {e}\n{traceback.format_exc()}")
        return True


def reformat_to_markdown_gemini(
    article_text: str,
    article_url: str,
    article_publish_date_str: str | None,
    image_urls: list[str] | None,
    api_key: str
) -> str | None:
    """
    Reformat article text to Markdown using Gemini API.
    Adds metadata (date, URL) and optionally includes images.
    Returns Markdown string or None on error.
    """
    if not api_key:
        logger.warning("No Gemini API key. Skipping Markdown reformatting.")
        return None

    if not article_text or not article_text.strip():
        logger.warning("Empty article text. Skipping Markdown reformatting.")
        return None

    try:
        genai.configure(api_key=api_key)
        model = genai.GenerativeModel('gemini-2.0-flash')

        prompt = (
            "You are an expert text reformatter. Convert the following article to clean, "
            "readable, well-structured Markdown. Preserve the original meaning and structure "
            "(headings, paragraphs, lists, blockquotes, code blocks). "
            "Do not add any commentary. Only output the Markdown. "
            "Remove obvious ads or promotional material. "
            "Generate an appropriate heading for the article based on the content.\n\n"
        )

        if image_urls:
            prompt += (
                "The following image URLs were extracted from the article. "
                "Include relevant ones using '![](image_url)' syntax where appropriate. "
                "Don't include all images if it clutters the article.\n"
                "Image URLs:\n"
            )
            for img_url in image_urls:
                prompt += f"- {img_url}\n"

        prompt += f"\nArticle Content to Reformat:\n{article_text}"

        logger.info(f"Sending article (len={len(article_text)}) to Gemini for Markdown reformatting")
        response = model.generate_content(prompt)

        if not response.candidates:
            logger.warning(f"Gemini (reformat): No candidates. Feedback: {response.prompt_feedback}")
            return None

        candidate = response.candidates[0]
        if not candidate.content or not candidate.content.parts:
            logger.warning(f"Gemini (reformat): No content. Finish: {candidate.finish_reason}")
            return None

        markdown_output = candidate.content.parts[0].text.strip()
        logger.info(f"Gemini Markdown reformatting successful. Length: {len(markdown_output)}")

        if not markdown_output:
            logger.warning("Gemini returned empty Markdown.")
            return None

        # Add metadata
        date_str = article_publish_date_str or "Date N/A"
        lines = markdown_output.split('\n')
        metadata_line = f"\n[{article_url}]({article_url}) - Published: {date_str}\n"

        if lines and lines[0].strip().startswith("#"):
            lines.insert(1, metadata_line)
            markdown_output = "\n".join(lines)
        else:
            markdown_output = metadata_line + markdown_output

        markdown_output += f"\n\n---\nOriginal article: [{article_url}]({article_url})"
        logger.info(f"Added metadata. Final length: {len(markdown_output)}")

        return markdown_output

    except Exception as e:
        logger.error(f"Error during Gemini Markdown reformatting: {e}\n{traceback.format_exc()}")
        return None


def convert_markdown_to_styled_html(
    markdown_string: str,
    font_size: str = "14pt",
    document_title: str = "Generated PDF Content"
) -> str:
    """
    Convert Markdown string to a full HTML document with embedded CSS styling.
    """
    if not markdown_string:
        logger.warning("Empty Markdown string. Returning empty HTML.")
        return ""

    css = f"""
        body {{
            font-family: sans-serif;
            line-height: 1.6;
            font-size: {font_size};
            margin: 2cm;
        }}
        h1, h2, h3, h4, h5, h6 {{
            margin-top: 1.5em;
            margin-bottom: 0.5em;
            line-height: 1.3;
        }}
        p {{
            margin-bottom: 1em;
        }}
        ul, ol {{
            margin-bottom: 1em;
            padding-left: 2em;
        }}
        li {{
            margin-bottom: 0.3em;
        }}
        blockquote {{
            margin-left: 1em;
            padding-left: 1em;
            border-left: 3px solid #eee;
            color: #555;
        }}
        pre {{
            background-color: #f5f5f5;
            padding: 1em;
            overflow-x: auto;
            white-space: pre-wrap;
            word-wrap: break-word;
            border-radius: 4px;
        }}
        code {{
            font-family: monospace;
            background-color: #f5f5f5;
            padding: 0.2em 0.4em;
            border-radius: 3px;
        }}
        pre code {{
            padding: 0;
            background-color: transparent;
            border-radius: 0;
        }}
        table {{
            border-collapse: collapse;
            width: 100%;
            margin-bottom: 1em;
        }}
        th, td {{
            border: 1px solid #ddd;
            padding: 8px;
            text-align: left;
        }}
        th {{
            background-color: #f2f2f2;
        }}
        img {{
            max-width: 100%;
            height: auto;
            display: block;
            margin: 1em 0;
        }}
    """

    logger.info(f"Converting Markdown to HTML. Font size: {font_size}")
    html_fragment = markdown2.markdown(
        markdown_string,
        extras=["fenced-code-blocks", "cuddled-lists", "tables", "strike"]
    )

    html_document = f"""
    <!DOCTYPE html>
    <html lang="en">
        <head>
            <meta charset="UTF-8">
            <title>{document_title}</title>
            <style>
                {css}
            </style>
        </head>
        <body>
            {html_fragment}
        </body>
    </html>
    """
    logger.info("Markdown to HTML conversion complete.")
    return html_document


def generate_pdf_from_html(html_content: str, output_pdf_path: str) -> bool:
    """
    Convert HTML content to PDF using Playwright (Chromium).
    Returns True on success, False on failure.
    """
    if not html_content:
        logger.error("Cannot generate PDF: HTML content is empty.")
        return False

    try:
        logger.info(f"Generating PDF: {output_pdf_path}")
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page()
            page.set_content(html_content, wait_until="networkidle")
            page.pdf(
                path=output_pdf_path,
                format="A4",
                margin={"top": "1cm", "bottom": "1cm", "left": "1cm", "right": "1cm"},
                print_background=True
            )
            browser.close()
        logger.info(f"PDF generated successfully: {output_pdf_path}")
        return True
    except Exception as e:
        logger.error(f"Failed to generate PDF: {e}\n{traceback.format_exc()}")
        return False


def _sanitize_title_for_filename(text: str | None, max_length: int = 50, default: str = "untitled") -> str:
    """
    Sanitize text for use in a filename.
    """
    if not text or not text.strip():
        return default
    sanitized = re.sub(r'[^\w\s-]', '', text.strip())
    sanitized = re.sub(r'\s+', '_', sanitized)
    return sanitized[:max_length]


def generate_pdf_filename(article_title: str, author_name: str | None = None) -> str:
    """
    Generate a PDF filename based on article title and optional author.
    Format: SanitizedTitle_Author.pdf or SanitizedTitle.pdf
    """
    sanitized_title = _sanitize_title_for_filename(article_title, max_length=75, default="Untitled_Article")
    sanitized_author = _sanitize_title_for_filename(author_name, max_length=25, default="")

    if sanitized_author:
        return f"{sanitized_title}_{sanitized_author}.pdf"
    return f"{sanitized_title}.pdf"


def upload_to_supernote(
    pdf_filepath: str,
    sn_email: str,
    sn_password: str,
    sn_target_path: str = "/Inbox/SendToSupernote"
) -> bool:
    """
    Upload a PDF file to Supernote cloud.
    Returns True on success, False on failure.
    """
    if not sn_email or not sn_password:
        logger.error("Supernote credentials not provided.")
        return False

    if not sn_target_path.startswith("/"):
        sn_target_path = "/" + sn_target_path

    target_folder_name = os.path.basename(sn_target_path)
    parent_path = os.path.dirname(sn_target_path)

    pdf_path = Path(pdf_filepath)
    if not pdf_path.exists():
        logger.error(f"PDF file not found: {pdf_filepath}")
        return False

    try:
        client = SNClient()
        logger.info(f"Logging in to Supernote cloud with email: {sn_email}")
        client.login(sn_email, sn_password)
        logger.info("Successfully logged in to Supernote cloud")

        # Check if target folder exists, create if not
        path_exists = False
        try:
            actual_parent = parent_path if parent_path else "/"
            items = client.ls(directory=actual_parent)

            for item in items:
                if item.file_name == target_folder_name and item.is_folder:
                    path_exists = True
                    logger.info(f"Found target folder: {sn_target_path}")
                    break

            if not path_exists:
                logger.info(f"Creating folder: {sn_target_path}")
                client.mkdir(target_folder_name, parent_path=parent_path)
                logger.info(f"Created folder: {sn_target_path}")
                path_exists = True

        except Exception as e_folder:
            logger.error(f"Error checking/creating folder: {e_folder}\n{traceback.format_exc()}")
            return False

        if not path_exists:
            logger.error(f"Could not confirm folder: {sn_target_path}")
            return False

        # Upload the PDF
        logger.info(f"Uploading {pdf_path.name} to {sn_target_path}...")
        client.put(file_path=pdf_path, parent=sn_target_path)
        logger.info(f"Successfully uploaded {pdf_path.name}")
        return True

    except Exception as e:
        logger.error(f"Error during Supernote upload: {e}\n{traceback.format_exc()}")
        return False


def process_url(
    url: str,
    gemini_api_key: str,
    sn_email: str,
    sn_password: str,
    sn_target_path: str = "/Inbox/SendToSupernote",
    font_size: str = "14pt",
    skip_quality_check: bool = False
) -> tuple[bool, str]:
    """
    Main processing pipeline: scrape URL, convert to PDF, upload to Supernote.

    Returns a tuple of (success: bool, message: str).
    """
    temp_dir = None
    try:
        # Step 1: Scrape content
        logger.info(f"Processing URL: {url}")
        scraped = scrape_article_content(url)
        if not scraped:
            return False, "Failed to scrape article content"

        # Step 2: Quality check (optional)
        if not skip_quality_check:
            is_quality = classify_article_quality(scraped['plain_text'], gemini_api_key)
            if not is_quality:
                return False, "Article classified as low-quality/advertisement"

        # Step 3: Reformat to Markdown
        markdown = reformat_to_markdown_gemini(
            scraped['plain_text'],
            url,
            scraped['extracted_date'],
            scraped['image_urls'],
            gemini_api_key
        )

        if not markdown:
            # Fallback: use plain HTML
            logger.warning("Markdown reformatting failed. Using cleaned HTML fallback.")
            html_content = scraped['html_content']
        else:
            # Step 4: Convert Markdown to styled HTML
            html_content = convert_markdown_to_styled_html(
                markdown,
                font_size=font_size,
                document_title=scraped['title']
            )

        if not html_content:
            return False, "Failed to generate HTML content"

        # Step 5: Generate PDF
        temp_dir = tempfile.mkdtemp(prefix="telegram_bot_pdf_")
        pdf_filename = generate_pdf_filename(scraped['title'], scraped['author'])
        pdf_path = os.path.join(temp_dir, pdf_filename)

        if not generate_pdf_from_html(html_content, pdf_path):
            return False, "Failed to generate PDF"

        # Step 6: Upload to Supernote
        if not upload_to_supernote(pdf_path, sn_email, sn_password, sn_target_path):
            return False, "Failed to upload to Supernote"

        return True, f"Successfully processed and uploaded: {scraped['title']}"

    except Exception as e:
        logger.error(f"Error processing URL {url}: {e}\n{traceback.format_exc()}")
        return False, f"Error: {str(e)}"

    finally:
        # Cleanup temp directory
        if temp_dir and os.path.exists(temp_dir):
            try:
                import shutil
                shutil.rmtree(temp_dir)
                logger.info(f"Cleaned up temp directory: {temp_dir}")
            except Exception as e_cleanup:
                logger.warning(f"Failed to cleanup temp dir: {e_cleanup}")
