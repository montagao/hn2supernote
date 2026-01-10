import logging
import traceback
import json
from trafilatura import extract as trafilatura_extract
from playwright.sync_api import sync_playwright, TimeoutError as PlaywrightTimeoutError
import os
import google.generativeai as genai
from datetime import datetime # For reformat_to_markdown_gemini publish_date handling
import markdown2
from bs4 import BeautifulSoup # Added for fallback title extraction
import re
from datetime import date as datetime_date # Alias to avoid confusion with datetime module
from pathlib import Path
from sncloud import SNClient
from urllib.parse import urljoin # Added for resolving relative image URLs

# Configure a logger for this module
logger = logging.getLogger(__name__)

# Min length for content to be considered valid after extraction
MIN_CONTENT_LENGTH = 150
HTML_SNIPPET_LENGTH = 1000 # For logging


def _extract_text_with_image_placeholders(soup: BeautifulSoup, base_url: str) -> str:
    """
    Extract text from a BeautifulSoup object while preserving image positions
    as Markdown-style placeholders. This ensures images stay in the correct
    order relative to the surrounding text.

    Returns plain text with embedded ![](image_url) placeholders.
    """
    result_parts = []

    def process_element(element):
        """Recursively process elements, preserving image positions."""
        if element.name is None:  # NavigableString (text node)
            text = str(element).strip()
            if text:
                result_parts.append(text)
        elif element.name == 'img':
            src = element.get('src')
            if src:
                absolute_src = urljoin(base_url, src)
                alt = element.get('alt', '')
                result_parts.append(f'\n\n![{alt}]({absolute_src})\n\n')
        elif element.name in ['script', 'style', 'noscript']:
            pass  # Skip these elements entirely
        elif element.name in ['br']:
            result_parts.append('\n')
        elif element.name in ['p', 'div', 'article', 'section', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'blockquote', 'li']:
            # Block-level elements: process children and add newlines
            for child in element.children:
                process_element(child)
            result_parts.append('\n\n')
        else:
            # Inline elements: just process children
            for child in element.children:
                process_element(child)

    # Process all children of the soup body (or the soup itself)
    body = soup.find('body') or soup
    for child in body.children:
        process_element(child)

    # Clean up excessive whitespace while preserving image placeholders
    text = ''.join(result_parts)
    # Normalize multiple newlines to max 2
    text = re.sub(r'\n{3,}', '\n\n', text)
    # Clean up spaces around image placeholders
    text = re.sub(r' +', ' ', text)

    return text.strip()


def scrape_article_content(url: str, raw_html_from_extension: str | None = None):
    """
    Scrape article content.
    If raw_html_from_extension is provided, it's assumed to be Readability.js output;
    plain text and title are extracted from it using BeautifulSoup.
    Otherwise, Playwright is used to fetch the page content, then Trafilatura (with fallbacks) extracts content.
    Returns a dictionary with 'title', 'html_content' (cleaned HTML for PDF fallback),
    'plain_text', 'extracted_date', and 'author', or None if scraping/extraction fails
    or content is insufficient.
    """
    logger.info(f"Starting content extraction for {url}")

    # Initialize fields
    title = "Untitled Article"
    plain_text = None
    cleaned_html_content = None # This will be the HTML used for PDF if Gemini fails
    extracted_date = None
    author = None
    image_urls = [] # Added to store extracted image URLs
    html_source_to_process = None # Will hold the full HTML if fetched, or the extension HTML

    if raw_html_from_extension:
        logger.info(f"Using raw HTML (expected from Readability.js) provided by extension for {url}. Length: {len(raw_html_from_extension)}")
        html_source_to_process = raw_html_from_extension
        cleaned_html_content = raw_html_from_extension # This is already the "main content" HTML

        try:
            soup = BeautifulSoup(html_source_to_process, 'html.parser')

            # Extract plain text WITH image placeholders to preserve ordering
            plain_text = _extract_text_with_image_placeholders(soup, url)
            if plain_text:
                logger.info(f"Extracted plain text with image placeholders from extension HTML. Length: {len(plain_text)}")
            else:
                logger.warning(f"Could not extract plain text from extension HTML for {url}")

            # Attempt to find a title from H1 or H2 in the Readability HTML
            h1_tag = soup.find('h1')
            if h1_tag and h1_tag.string:
                title = h1_tag.string.strip()
                logger.info(f"Extracted title from H1 in extension HTML: '{title}'")
            else:
                h2_tag = soup.find('h2')
                if h2_tag and h2_tag.string:
                    title = h2_tag.string.strip()
                    logger.info(f"Extracted title from H2 in extension HTML: '{title}'")
                else:
                    logger.info(f"No H1 or H2 found for title in extension HTML for {url}. Title remains '{title}'.")

            # Extract image URLs from Readability HTML (for reference, positions are now in plain_text)
            try:
                images = soup.find_all('img')
                for img in images:
                    src = img.get('src')
                    if src:
                        # Convert to absolute URL if necessary
                        absolute_src = urljoin(url, src)
                        image_urls.append(absolute_src)
                if image_urls:
                    logger.info(f"Extracted {len(image_urls)} image URLs from extension HTML for {url}. First few: {image_urls[:3]}")
            except Exception as e_img_extract_ext:
                logger.warning(f"Could not extract images from extension HTML for {url}: {e_img_extract_ext}")

            # Date and Author are generally not available from Readability's basic .content output
            # So extracted_date and author will remain None for this path

        except Exception as e_bs_ext_html:
            logger.error(f"Error processing HTML from extension for {url}: {e_bs_ext_html}\n{traceback.format_exc()}")
            # If there's an error here, plain_text or title might be None/default

        # Check content sufficiency from extension HTML
        if not plain_text or len(plain_text) < MIN_CONTENT_LENGTH:
            logger.error(f"FAIL {url}: Plain text from extension HTML is insufficient (length: {len(plain_text) if plain_text else 0}, min: {MIN_CONTENT_LENGTH}).")
            return None
        
        logger.info(f"Successfully processed content from extension HTML for {url}. Title: '{title}'. Plain text length: {len(plain_text)}.")
        return {
            'title': title,
            'html_content': cleaned_html_content, # This is the Readability HTML itself
            'plain_text': plain_text,
            'extracted_date': extracted_date, # Will be None
            'author': author, # Will be None
            'image_urls': image_urls # Added
        }
    
    # --- Fallback to Playwright and Trafilatura if no extension HTML provided ---
    else:
        logger.info(f"No raw HTML provided. Fetching article from {url} using Playwright & Trafilatura pipeline.")
        try:
            with sync_playwright() as p:
                browser = p.firefox.launch(
                    headless=True,
                    timeout=60000
                )
                page = browser.new_page()
                page.set_default_navigation_timeout(45000)
                page.set_default_timeout(30000)
                logger.info(f"Playwright: Navigating to {url}")
                page.goto(url, wait_until="networkidle", timeout=45000)
                logger.info(f"Playwright: Page loaded. Extracting page content for {url}")
                html_source_to_process = page.content()
                logger.info(f"Playwright: Successfully fetched HTML source. Length: {len(html_source_to_process)}")
                browser.close()
        except PlaywrightTimeoutError as pte:
            logger.error(f"FAIL {url}: Playwright navigation/load timeout: {pte}")
            return None
        except Exception as e_pw:
            logger.error(f"FAIL {url}: Playwright failed: {e_pw}\n{traceback.format_exc()}")
            return None

        if not html_source_to_process:
            logger.error(f"FAIL {url}: HTML source is empty after Playwright fetch.")
            return None
        
        logger.debug(f"Full HTML source for {url} (first {HTML_SNIPPET_LENGTH} chars): {html_source_to_process[:HTML_SNIPPET_LENGTH]}")

        # --- Trafilatura Pipeline (with its own fallbacks) --- 
        try:
            metadata_extraction_settings = {
                'output_format': 'json',
                'with_metadata': True,
                'include_comments': False,
                'include_tables': True,
                'include_formatting': True,
                'favor_recall': True
            }
            logger.info(f"Trafilatura (JSON): Attempting primary extraction for {url} with settings: {metadata_extraction_settings}")
            json_string_data = trafilatura_extract(html_source_to_process, **metadata_extraction_settings)

            if json_string_data:
                try:
                    extracted_data = json.loads(json_string_data)
                    if extracted_data and isinstance(extracted_data, dict):
                        plain_text = extracted_data.get('text')
                        title = extracted_data.get('title') or title
                        extracted_date = extracted_data.get('date')
                        author = extracted_data.get('author')
                        logger.info(f"Trafilatura (JSON) for {url}: Extracted title: '{title}'. Plain text length: {len(plain_text) if plain_text else 0}")
                    else:
                        logger.warning(f"Trafilatura (JSON) for {url}: Parsed data is not a valid dictionary or is empty. Data: {extracted_data}")
                except json.JSONDecodeError as e_json:
                    logger.warning(f"Trafilatura (JSON) for {url}: Failed to parse JSON: {e_json}. Raw JSON string (first {HTML_SNIPPET_LENGTH} chars): {json_string_data[:HTML_SNIPPET_LENGTH]}")
            else:
                logger.warning(f"Trafilatura (JSON) for {url}: Returned no data. HTML source snippet (first {HTML_SNIPPET_LENGTH} chars): {html_source_to_process[:HTML_SNIPPET_LENGTH]}")
        except Exception as e_json_extract:
            logger.error(f"FAIL {url}: Error during Trafilatura JSON extraction: {e_json_extract}\n{traceback.format_exc()}")

        plain_text_len = len(plain_text) if plain_text else 0
        if not plain_text or plain_text_len < MIN_CONTENT_LENGTH:
            reason_for_fallback = "plain_text is None" if not plain_text else f"plain_text too short (length: {plain_text_len})"
            logger.info(f"Trafilatura (Text Fallback) for {url}: Triggering due to: {reason_for_fallback}. Attempting text-only fallback.")
            try:
                text_fallback_settings = {
                    'output_format': 'txt',
                    'include_comments': False,
                    'include_tables': True,
                    'favor_recall': True
                }
                logger.info(f"Trafilatura (Text Fallback) for {url}: Using settings: {text_fallback_settings}")
                plain_text_fallback = trafilatura_extract(html_source_to_process, **text_fallback_settings)
                if plain_text_fallback and len(plain_text_fallback) >= MIN_CONTENT_LENGTH:
                    plain_text = plain_text_fallback
                    logger.info(f"Trafilatura (Text Fallback) for {url}: Successfully extracted text. Length: {len(plain_text)}")
                elif plain_text_fallback:
                    logger.warning(f"Trafilatura (Text Fallback) for {url}: Extracted text too short (length: {len(plain_text_fallback)}). HTML snippet: {html_source_to_process[:HTML_SNIPPET_LENGTH]}")
                else:
                    logger.warning(f"Trafilatura (Text Fallback) for {url}: Returned no text. HTML snippet: {html_source_to_process[:HTML_SNIPPET_LENGTH]}")
            except Exception as e_txt_extract:
                logger.error(f"FAIL {url}: Error during Trafilatura text-only fallback extraction: {e_txt_extract}\n{traceback.format_exc()}")

        if not title or title == "Untitled Article" or title.strip() == "":
            reason_for_title_fallback = "title is None/empty" if not title or title.strip() == "" else "title is 'Untitled Article'"
            logger.info(f"BeautifulSoup (Title Fallback) for {url}: Triggering due to: {reason_for_title_fallback}. Attempting BeautifulSoup title extraction.")
            logger.debug(f"BeautifulSoup (Title Fallback) for {url}: Parsing HTML (first {HTML_SNIPPET_LENGTH} chars): {html_source_to_process[:HTML_SNIPPET_LENGTH]}")
            try:
                soup = BeautifulSoup(html_source_to_process, 'html.parser')
                if soup.title and soup.title.string:
                    title = soup.title.string.strip()
                    if title:
                        logger.info(f"BeautifulSoup (Title Fallback) for {url}: Extracted title: '{title}'")
                    else:
                        title = "Untitled Article"
                        logger.warning(f"BeautifulSoup (Title Fallback) for {url}: <title> tag was present but its string content was empty after stripping. Reset to 'Untitled Article'.")    
                else:
                    logger.warning(f"BeautifulSoup (Title Fallback) for {url}: No <title> tag found or it's empty.")
            except Exception as e_bs_title:
                logger.error(f"FAIL {url}: Error during BeautifulSoup title extraction: {e_bs_title}\n{traceback.format_exc()}")
        
        if not title or title.strip() == "":
            logger.warning(f"URL {url}: Title remains empty or generic after all fallbacks. Setting to 'Untitled Article'.")
            title = "Untitled Article"

        if not plain_text or len(plain_text) < MIN_CONTENT_LENGTH:
            logger.error(f"FAIL {url}: After all Trafilatura fallbacks, extracted plain text is still insufficient (length: {len(plain_text) if plain_text else 0}, min: {MIN_CONTENT_LENGTH}). Not processing further.")
            return None

        # Trafilatura for cleaned HTML content (for PDF fallback if Markdown fails)
        # This is the 'html_content' that will be returned by the function if this path is taken.
        try:
            html_extraction_settings = {
                'output_format': 'html',
                'include_comments': False,
                'include_tables': True,
                'include_formatting': True,
                'favor_recall': True
            }
            logger.info(f"Trafilatura (HTML): Extracting cleaned HTML for {url} with settings: {html_extraction_settings}")
            cleaned_html_content = trafilatura_extract(html_source_to_process, **html_extraction_settings)
            if not cleaned_html_content:
                logger.warning(f"WARN {url}: Trafilatura (HTML extract) returned no cleaned HTML. Constructing basic HTML from plain text for PDF fallback.")
                escaped_title = title.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
                escaped_plain_text_as_html = plain_text.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;').replace('\n', '<br />\n')
                cleaned_html_content = f"<!DOCTYPE html><html><head><title>{escaped_title}</title></head><body><h1>{escaped_title}</h1><div>{escaped_plain_text_as_html}</div></body></html>"
            else:
                logger.info(f"Trafilatura (HTML): Successfully extracted cleaned HTML. Length: {len(cleaned_html_content)}")
        except Exception as e_html_extract:
            logger.error(f"FAIL {url}: Error during Trafilatura cleaned HTML extraction: {e_html_extract}\n{traceback.format_exc()}")
            logger.warning(f"WARN {url}: Constructing minimal HTML for PDF due to cleaned HTML extraction error.")
            escaped_title = title.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
            escaped_plain_text_as_html = plain_text.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;').replace('\n', '<br />\n')
            cleaned_html_content = f"<!DOCTYPE html><html><head><title>{escaped_title}</title></head><body><h1>{escaped_title}</h1><div>{escaped_plain_text_as_html}</div></body></html>"

        # After cleaned_html_content is finalized, extract text with image placeholders
        # This ensures images are in the correct position relative to surrounding text
        if cleaned_html_content:
            try:
                soup_for_images = BeautifulSoup(cleaned_html_content, 'html.parser')
                # Re-extract plain_text with image placeholders for proper ordering
                plain_text_with_images = _extract_text_with_image_placeholders(soup_for_images, url)
                if plain_text_with_images and len(plain_text_with_images) >= MIN_CONTENT_LENGTH:
                    plain_text = plain_text_with_images
                    logger.info(f"Re-extracted plain text with image placeholders. Length: {len(plain_text)}")

                # Also collect image URLs for reference
                imgs = soup_for_images.find_all('img')
                for img_tag in imgs:
                    src = img_tag.get('src')
                    if src:
                        # Convert to absolute URL. url is the base URL of the article page.
                        absolute_src = urljoin(url, src)
                        if absolute_src not in image_urls: # Avoid duplicates if already added (e.g. if combining strategies)
                             image_urls.append(absolute_src)
                if image_urls:
                    logger.info(f"Extracted {len(image_urls)} image URLs from final cleaned HTML for {url}. First few: {image_urls[:3]}")
            except Exception as e_final_img_extract:
                logger.warning(f"Could not extract images from final cleaned_html_content for {url}: {e_final_img_extract}")

        logger.info(f"Successfully processed content via Playwright/Trafilatura for {url}. Final Title: '{title}'. Plain text length: {len(plain_text) if plain_text else 0}.")
        return {
            'title': title,
            'html_content': cleaned_html_content,
            'plain_text': plain_text,
            'extracted_date': extracted_date,
            'author': author,
            'image_urls': image_urls # Added
        }

if __name__ == '__main__':
    # Example usage for testing this module directly
    logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(name)s - %(levelname)s - %(message)s')
    
    # Test with a URL that requires Playwright
    # test_url_dynamic = "https://www.forbes.com/sites/arilevy/2024/07/22/microsoft-azure-earnings-q4-2024/"
    # logger.info(f"--- Testing Playwright + Trafilatura for: {test_url_dynamic} ---")
    # scraped_data_dynamic = scrape_article_content(test_url_dynamic)
    # if scraped_data_dynamic:
    #     logger.info(f"Title: {scraped_data_dynamic['title']}")
    #     logger.info(f"Plain text length: {len(scraped_data_dynamic['plain_text'])}")
    #     logger.info(f"HTML content length: {len(scraped_data_dynamic['html_content'])}")
    # else:
    #     logger.error(f"Scraping failed for {test_url_dynamic}")

    # Test with providing raw HTML (e.g., from extension)
    test_url_static = "http://example.com" # URL is for context, HTML is primary
    sample_html = """
    <!DOCTYPE html><html><head><title>Test Page</title></head>
    <body><h1>Main Heading</h1><p>This is a paragraph with some content.</p>
    <article><p>This is article content.</p></article>
    </body></html>
    """
    logger.info(f"--- Testing Trafilatura with provided HTML for: {test_url_static} ---")
    scraped_data_static = scrape_article_content(test_url_static, raw_html_from_extension=sample_html)
    if scraped_data_static:
        logger.info(f"Title: {scraped_data_static['title']}")
        logger.info(f"Plain text: {scraped_data_static['plain_text']}")
        logger.info(f"Image URLs: {scraped_data_static['image_urls']}") # Log image URLs
    else:
        logger.error(f"Scraping failed for {test_url_static} with provided HTML")

# --- Gemini API Functions ---
def classify_article_quality(article_text: str) -> bool:
    """
    Classifies article quality using Gemini API.
    Returns True if thought-provoking, False if advertisement/low-quality.
    Defaults to True if API key is missing or an error occurs.
    Assumes genai.configure(api_key=os.getenv("GEMINI_API_KEY")) has been called elsewhere.
    """
    api_key = os.getenv("GEMINI_API_KEY")
    if not api_key:
        logger.warning("GEMINI_API_KEY not found. Skipping AI classification, defaulting to 'good'.")
        return True

    try:
        model = genai.GenerativeModel('gemini-3-flash-preview') # Using a common model, adjust if needed
        prompt = (
            "You are an expert content quality analyst. Your task is to classify an article based on its content. "
            "Determine if the article is a substantive, thought-provoking piece that offers insights, analysis, or in-depth information. "
            "Alternatively, determine if it is primarily an advertisement, promotional material, very superficial content, a product announcement without deeper substance, or a low-quality piece. "
            "Respond with only one of these two words: 'thought-provoking' or 'advertisement'.\n\n"
            "Article Content (first 10000 characters):\n"
            f"{article_text[:10000]}"  # Truncate to a reasonable length
        )

        response = model.generate_content(prompt)

        if not response.candidates:
            logger.warning(f"Gemini (classify) returned no candidates. Feedback: {response.prompt_feedback}")
            return True
        
        candidate = response.candidates[0]
        if not candidate.content or not candidate.content.parts:
            logger.warning(f"Gemini (classify) first candidate has no content/parts. Finish: {candidate.finish_reason}, Safety: {candidate.safety_ratings}")
            return True

        response_text = candidate.content.parts[0].text.strip().lower()
        logger.info(f"Gemini classification raw response: '{response_text}'")

        if "thought-provoking" in response_text:
            return True
        elif "advertisement" in response_text:
            return False
        else:
            logger.warning(f"Gemini classification response '{response_text}' was not definitive. Defaulting to 'good'.")
            return True

    except ImportError:
        logger.error("google-generativeai library not installed or not found. Skipping AI classification.")
        return True
    except Exception as e:
        logger.error(f"Error during Gemini API call for classification: {e}\n{traceback.format_exc()}")
        return True

def reformat_to_markdown_gemini(article_text: str, article_url: str, article_publish_date_str: str | None, image_urls: list[str] | None = None) -> str | None:
    """
    Reformats article text to Markdown using Gemini API, adds date/URL near top, and appends source URL.
    The article_text may contain inline image placeholders like ![alt](url) which should be preserved
    in their original positions relative to the surrounding text.
    Returns Markdown string or None if an error occurs or API key is missing.
    Assumes genai.configure(api_key=os.getenv("GEMINI_API_KEY")) has been called elsewhere.
    article_publish_date_str should be a string representation of the date if available.
    image_urls is kept for backward compatibility but images are now embedded inline in article_text.
    """
    api_key = os.getenv("GEMINI_API_KEY")
    if not api_key:
        logger.warning("GEMINI_API_KEY not found. Skipping Gemini Markdown reformatting.")
        return None

    if not article_text or not article_text.strip():
        logger.warning("Article text is empty. Skipping Gemini Markdown reformatting.")
        return None

    try:
        model = genai.GenerativeModel('gemini-3-flash-preview')
        instructional_prompt = (
            "You are an expert text reformatter. Your task is to convert the following article content into clean, readable, and well-structured Markdown. "
            "Focus on preserving the original meaning and structure (headings, paragraphs, lists, blockquotes, code blocks if any) as much as possible. "
            "Do not add any commentary, preamble, or explanation of your own. Only output the Markdown. "
            "Ensure that the Markdown is suitable for direct conversion to HTML and then to PDF.\n\n"
            "Make sure to remove any obvious ads or promotional material. "
            "Make sure to generate a heading for the article based on the content. "
            "\n\nIMPORTANT: The article content contains image placeholders in Markdown format like ![alt text](image_url). "
            "These images are already positioned in the correct location relative to the surrounding text. "
            "You MUST preserve these image placeholders in their exact positions. Do not move, reorder, or remove them "
            "unless they appear to be advertisements or decorative elements unrelated to the article content. "
        )

        instructional_prompt += "\nArticle Content to Reformat:\n"

        contents_for_gemini = [instructional_prompt, article_text]

        # Count embedded images for logging
        embedded_image_count = article_text.count('![')
        logger.info(f"Sending article of length {len(article_text)} to Gemini for Markdown reformatting. {embedded_image_count} embedded images found.")
        response = model.generate_content(contents_for_gemini)

        if not response.candidates:
            logger.warning(f"Gemini (reformat) returned no candidates. Feedback: {response.prompt_feedback}")
            return None
        
        candidate = response.candidates[0]
        if not candidate.content or not candidate.content.parts:
            logger.warning(f"Gemini (reformat) first candidate has no content/parts. Finish: {candidate.finish_reason}, Safety: {candidate.safety_ratings}")
            return None

        markdown_output = candidate.content.parts[0].text.strip()
        logger.info(f"Gemini Markdown reformatting successful. Output length: {len(markdown_output)}")
        if not markdown_output:
            logger.warning("Gemini returned empty Markdown. Treating as failure.")
            return None
        
        date_str_to_insert = article_publish_date_str if article_publish_date_str else "Date N/A"

        lines = markdown_output.split('\n')
        metadata_line = f"\n[{article_url}]({article_url}) - Published: {date_str_to_insert}\n"
        if lines and lines[0].strip().startswith("#"):
            lines.insert(1, metadata_line)
            markdown_output = "\n".join(lines)
        else:
            markdown_output = metadata_line + markdown_output
        
        markdown_output += f"\n\n---\nOriginal article: [{article_url}]({article_url})"
        logger.info(f"Added metadata and source URL to Gemini Markdown. Total length: {len(markdown_output)}")

        return markdown_output

    except ImportError:
        logger.error("google-generativeai library not installed or not found. Skipping Gemini Markdown reformatting.")
        return None
    except Exception as e:
        logger.error(f"Error during Gemini API call for Markdown reformatting: {e}\n{traceback.format_exc()}")
        return None

# --- PDF Generation Functions ---
def convert_markdown_to_styled_html(markdown_string: str, font_size: str = "14pt", document_title: str = "Generated PDF Content", return_css_only: bool = False) -> str:
    """
    Converts Markdown string to a full HTML document with embedded font style,
    or returns just the CSS string if return_css_only is True.
    """
    # Define CSS once
    markdown_css = f"""
        body {{
            font-family: sans-serif;
            line-height: 1.6;
            font-size: {font_size};
            margin: 2cm; /* Add some margins for better PDF layout */
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
            margin-left: 1em; /* Reduced margin slightly */
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
            border-radius: 4px; /* Added border-radius */
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
            display: block; /* Avoid extra space below images */
            margin: 1em 0; /* Add some margin around images */
        }}
    """

    if return_css_only:
        logger.info(f"Returning CSS only. Font size: {font_size}")
        return markdown_css

    if not markdown_string:
        logger.warning("Markdown string is empty. Returning empty HTML string.")
        return ""
    
    logger.info(f"Converting Markdown to HTML. Font size: {font_size}, Title: {document_title}")
    html_fragment = markdown2.markdown(markdown_string, extras=["fenced-code-blocks", "cuddled-lists", "tables", "strike"])
    
    styled_html_document = f"""
    <!DOCTYPE html>
    <html lang="en">
        <head>
            <meta charset="UTF-8">
            <title>{document_title}</title>
            <style>
                {markdown_css}
            </style>
        </head>
        <body>
            {html_fragment}
        </body>
    </html>
    """
    logger.info("Successfully converted Markdown to styled HTML.")
    return styled_html_document

def generate_pdf_from_html(html_content: str, output_pdf_path: str) -> bool:
    """
    Convert final HTML content to PDF using Playwright (Chromium).
    output_pdf_path is the full path where the PDF should be saved.
    """
    if not html_content:
        logger.error("Cannot generate PDF: HTML content is empty.")
        return False

    try:
        logger.info(f"Generating PDF with Playwright: {output_pdf_path}")
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
        logger.error(f"Failed to generate PDF '{output_pdf_path}': {e}\n{traceback.format_exc()}")
        return False

# --- Supernote Upload Functions ---
def _sanitize_title_for_filename(text: str | None, max_length: int = 50, default_on_none: str = "untitled") -> str:
    """
    Sanitize the text for use in a filename.
    Replaces spaces with underscores, removes non-alphanumeric characters (except _ and -),
    and truncates to max_length. If text is None or empty, returns default_on_none.
    """
    if not text or not text.strip():
        return default_on_none
    sanitized = re.sub(r'[^\w\s-]', '', text.strip())
    sanitized = re.sub(r'\s+', '_', sanitized)
    return sanitized[:max_length]

def generate_supernote_pdf_filename(article_title: str, author_name: str | None = None) -> str:
    """
    Generate PDF filename for Supernote based on article title and author name.
    Format: SanitizedArticleTitle_SanitizedAuthorName.pdf or SanitizedArticleTitle.pdf
    """
    # Allow more length for the main title part
    sanitized_title = _sanitize_title_for_filename(article_title, max_length=75, default_on_none="Untitled_Article")
    
    # Sanitize author, default to empty string if None or empty, so it can be omitted easily
    sanitized_author = _sanitize_title_for_filename(author_name, max_length=25, default_on_none="") 

    if sanitized_author: # Only append author if it's not an empty string after sanitization
        return f"{sanitized_title}_{sanitized_author}.pdf"
    return f"{sanitized_title}.pdf"

def upload_pdfs_to_supernote(pdf_filepaths: list[str], sn_email: str, sn_password: str, sn_target_path: str | None = None) -> int:
    """
    Upload PDF files to the specified path on Supernote using sncloud.
    Credentials (sn_email, sn_password) are passed directly.
    sn_target_path can be passed, otherwise defaults to SUPERNOTE_TARGET_PATH env var or /Inbox/SendToSupernote.
    Returns the number of successfully uploaded files.
    """
    test_mode_val = os.getenv("TEST_MODE", "False").lower()
    test_mode = test_mode_val in ("true", "1", "t", "yes")

    if test_mode:
        logger.info(f"TEST MODE: Skipping actual upload to Supernote for {len(pdf_filepaths)} files.")
        if pdf_filepaths:
            logger.info(f"Would have uploaded: {', '.join(pdf_filepaths)}")
        return len(pdf_filepaths)

    if not sn_email or not sn_password:
        logger.error("Supernote email or password not provided for upload.")
        return 0

    target_path_str = sn_target_path or os.getenv("SUPERNOTE_TARGET_PATH", "/Inbox/SendToSupernote")
    if not target_path_str.startswith("/"):
        target_path_str = "/" + target_path_str
        logger.info(f"Corrected Supernote target path to: {target_path_str}")
    
    target_folder_name = os.path.basename(target_path_str)
    parent_path_str = os.path.dirname(target_path_str)

    try:
        client = SNClient()
        logger.info(f"Logging in to Supernote cloud with email: {sn_email}")
        client.login(sn_email, sn_password)
        logger.info("Successfully logged in to Supernote cloud")

        path_exists = False
        try:
            # Ensure parent_path_str is not empty if target_path_str is a root folder like "/MyFolder"
            # If parent_path_str is empty (e.g. target is "/ExistingFolder"), ls works on "/"
            # If target is "/NewFolderAtRoot", parent is "/", ls on "/" is correct.
            # If target is "/ExistingFolder/NewSubFolder", parent is "/ExistingFolder", ls on parent is correct.
            actual_parent_for_ls = parent_path_str if parent_path_str else "/"
            
            current_path_items = client.ls(directory=actual_parent_for_ls)
            logger.debug(f"Items in '{actual_parent_for_ls}': {[item.file_name for item in current_path_items]}")

            for item in current_path_items:
                if item.file_name == target_folder_name and item.is_folder:
                    path_exists = True
                    logger.info(f"Found target folder: {target_path_str}")
                    break
            
            if not path_exists:
                logger.info(f"Target folder '{target_folder_name}' not found in '{parent_path_str}'. Attempting to create it.")
                # client.mkdir expects parent_path to be the directory *containing* the new folder.
                # If target_path_str = "/MyNewFolder", parent_path_str = "/".
                # If target_path_str = "/ExistingFolder/MyNewSubfolder", parent_path_str = "/ExistingFolder".
                client.mkdir(target_folder_name, parent_path=parent_path_str)
                logger.info(f"Successfully created folder: {target_path_str}")
                path_exists = True # Assume creation was successful

        except Exception as e_folder_check:
            logger.error(f"Error while checking or creating target folder '{target_path_str}': {e_folder_check}\n{traceback.format_exc()}")
            logger.error(f"Please ensure the base path '{parent_path_str}' exists or create '{target_path_str}' manually.")
            return 0 # Critical error if folder cannot be assured

        if not path_exists:
             logger.error(f"Target folder '{target_path_str}' could not be confirmed or created.")
             return 0

        uploaded_count = 0
        for pdf_filepath_str in pdf_filepaths:
            pdf_path_obj = Path(pdf_filepath_str)
            if not pdf_path_obj.exists():
                logger.error(f"PDF file not found: {pdf_filepath_str}. Skipping upload.")
                continue
            try:
                logger.info(f"Uploading {pdf_path_obj.name} to Supernote path '{target_path_str}'...")
                client.put(file_path=pdf_path_obj, parent=target_path_str)
                logger.info(f"Successfully uploaded {pdf_path_obj.name}")
                uploaded_count += 1
            except Exception as e_upload:
                logger.error(f"ERROR uploading {pdf_path_obj.name}: {e_upload}\n{traceback.format_exc()}")
        return uploaded_count

    except Exception as e_sn_process:
        logger.error(f"ERROR in Supernote upload process: {e_sn_process}\n{traceback.format_exc()}")
        return 0 