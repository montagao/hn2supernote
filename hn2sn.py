import feedparser
import os
import logging
import traceback
from datetime import date
from dotenv import load_dotenv
from weasyprint import HTML
from sncloud import SNClient
from pathlib import Path
import re
import google.generativeai as genai
import markdown2 # For Markdown to HTML conversion
import json # For parsing Trafilatura JSON output
import xml.etree.ElementTree as ET # For direct XML parsing of OPML
from dateutil import parser as date_parser # For parsing dates from feeds
from datetime import datetime # For fallback dates and timezone awareness

# New imports for scraping
from trafilatura import extract as trafilatura_extract
from playwright.sync_api import sync_playwright, TimeoutError as PlaywrightTimeoutError

load_dotenv()

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(levelname)s - %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)

# Min length for content to be considered valid after extraction
MIN_CONTENT_LENGTH = 150 

# Configuration for OPML processing
OPML_FILE_PATH = os.getenv("OPML_FILE_PATH")
MAX_ITEMS_PER_FEED = int(os.getenv("MAX_ITEMS_PER_FEED", 5)) # Default to 5 items per feed
HISTORY_FILE = os.getenv("HISTORY_FILE", "processed_articles.log") # For tracking processed articles
MAX_TOTAL_ARTICLES = int(os.getenv("MAX_TOTAL_ARTICLES", 0)) # 0 means no global limit unless specified

def log(message):
    """Log a message to both console and log file"""
    logging.info(message)


# Removed COUNT and MIN_POINTS as they were HN specific


def get_articles_from_opml():
    """
    Parses OPML, collects all articles from feeds, filters by history, 
    sorts by date, then limits to MAX_TOTAL_ARTICLES.
    Returns a list of dicts and skipped_from_history_count.
    """
    if not OPML_FILE_PATH:
        log("ERROR: OPML_FILE_PATH not set. Cannot fetch articles.")
        return [], 0

    if not os.path.exists(OPML_FILE_PATH):
        log(f"ERROR: OPML file not found at {OPML_FILE_PATH}")
        return [], 0

    log(f"Parsing OPML file with ElementTree: {OPML_FILE_PATH}")
    opml_feeds_to_process = []
    try:
        tree = ET.parse(OPML_FILE_PATH)
        root = tree.getroot()
        # Find all <outline> elements within <body> that have an xmlUrl attribute
        for outline_element in root.findall("./body/outline[@xmlUrl]"):
            feed_url = outline_element.get('xmlUrl')
            feed_title = outline_element.get('title', outline_element.get('text', "Untitled Feed")) # Use title, fallback to text
            if feed_url:
                opml_feeds_to_process.append({'url': feed_url, 'title': feed_title})
        log(f"ElementTree: Found {len(opml_feeds_to_process)} feeds with xmlUrl in OPML.")

    except ET.ParseError as e_xml_parse:
        log(f"ERROR: Failed to parse OPML file {OPML_FILE_PATH} with ElementTree (XML ParseError): {e_xml_parse}")
        return [], 0
    except Exception as e_et:
        log(f"ERROR: Failed during ElementTree OPML processing for {OPML_FILE_PATH}: {e_et}")
        log(traceback.format_exc())
        return [], 0

    if not opml_feeds_to_process:
        log(f"No feeds with xmlUrl found in OPML file using ElementTree: {OPML_FILE_PATH}")
        return [], 0

    all_fetched_articles = [] # Temp list to hold all articles from all feeds before sorting/limiting
    current_run_unique_links = set()
    history_processed_links = set()
    articles_skipped_from_history_count = 0

    if os.path.exists(HISTORY_FILE):
        try:
            with open(HISTORY_FILE, 'r') as hf:
                history_processed_links = set(line.strip() for line in hf if line.strip())
            log(f"Loaded {len(history_processed_links)} links from history file: {HISTORY_FILE}")
        except Exception as e_hist_read:
            log(f"WARN: Could not read history file {HISTORY_FILE}: {e_hist_read}. Proceeding without history.")

    log(f"Found {len(opml_feeds_to_process)} feeds. Initial fetch (up to {MAX_ITEMS_PER_FEED} per feed then global sort/limit)...")

    for feed_info in opml_feeds_to_process:
        feed_url = feed_info['url']
        feed_title_from_opml = feed_info['title']
        log(f"Fetching feed: '{feed_title_from_opml}' from {feed_url}")
        try:
            parsed_feed = feedparser.parse(feed_url)
            if parsed_feed.bozo:
                log(f"WARN: Feed '{feed_title_from_opml}' ({feed_url}) may be ill-formed: {parsed_feed.bozo_exception}")
            
            items_from_this_feed_collected = 0
            for entry in parsed_feed.entries:
                if items_from_this_feed_collected >= MAX_ITEMS_PER_FEED:
                    log(f"Reached MAX_ITEMS_PER_FEED ({MAX_ITEMS_PER_FEED}) for '{feed_title_from_opml}'. Moving to next feed.")
                    break

                article_link = entry.get('link')
                article_title_from_feed = entry.get("title", "Untitled Article from Feed")

                if not article_link:
                    log(f"WARN: Entry in '{feed_title_from_opml}' missing link. Title: '{article_title_from_feed}'")
                    continue
                
                # Check against history and current run unique links *before* adding to all_fetched_articles
                if article_link in history_processed_links:
                    log(f"INFO: Skipping (history): '{article_link}' from '{feed_title_from_opml}'.")
                    articles_skipped_from_history_count += 1
                    continue
                
                if article_link in current_run_unique_links:
                    log(f"DEBUG: Skipping (already collected this run): '{article_link}' from '{feed_title_from_opml}'.")
                    continue

                # Attempt to parse publication date
                publish_date_str = entry.get('published', entry.get('updated'))
                parsed_date = None
                if publish_date_str:
                    try:
                        parsed_date = date_parser.parse(publish_date_str)
                        # Convert to offset-aware if naive, assuming UTC if no tzinfo for consistent sorting
                        if parsed_date.tzinfo is None or parsed_date.tzinfo.utcoffset(parsed_date) is None:
                            # log(f"DEBUG: Date for '{article_link}' is naive, assuming UTC.")
                            parsed_date = parsed_date.replace(tzinfo=datetime.timezone.utc)
                    except Exception as e_date:
                        log(f"WARN: Could not parse date '{publish_date_str}' for '{article_link}': {e_date}")
                        parsed_date = datetime.now(datetime.timezone.utc) # Fallback to now for sorting, or handle as None
                else:
                    log(f"WARN: No publish date for '{article_link}', using current time for sorting.")
                    parsed_date = datetime.now(datetime.timezone.utc) # Fallback
                
                all_fetched_articles.append({
                    'link': article_link,
                    'source_feed_title': feed_title_from_opml,
                    'article_title_from_feed': article_title_from_feed,
                    'publish_date': parsed_date
                })
                current_run_unique_links.add(article_link)
                items_from_this_feed_collected +=1

        except Exception as e:
            log(f"ERROR: Failed to fetch/parse feed '{feed_title_from_opml}' ({feed_url}): {e}")
            # log(traceback.format_exc()) # Optionally disable for cleaner logs if too verbose

    # Sort all collected unique new articles by date (newest first)
    all_fetched_articles.sort(key=lambda x: x['publish_date'], reverse=True)
    log(f"Collected {len(all_fetched_articles)} unique new articles from all feeds. Now applying MAX_TOTAL_ARTICLES limit.")

    articles_to_process = []
    if MAX_TOTAL_ARTICLES > 0 and len(all_fetched_articles) > MAX_TOTAL_ARTICLES:
        articles_to_process = all_fetched_articles[:MAX_TOTAL_ARTICLES]
        log(f"Limited to MAX_TOTAL_ARTICLES: {MAX_TOTAL_ARTICLES}. Original count: {len(all_fetched_articles)}.")
    else:
        articles_to_process = all_fetched_articles
        if MAX_TOTAL_ARTICLES > 0:
             log(f"Total new articles ({len(articles_to_process)}) is within MAX_TOTAL_ARTICLES limit ({MAX_TOTAL_ARTICLES}).")

    log(f"Total articles selected for processing this run: {len(articles_to_process)}")
    log(f"Skipped {articles_skipped_from_history_count} articles from history file during initial scan.")
    return articles_to_process, articles_skipped_from_history_count


def scrape(url):
    """
    Scrape article content using Playwright to fetch and Trafilatura to extract.
    Returns a dictionary with 'title', 'html_content' (cleaned HTML), 
    and 'plain_text' or None if scraping fails or content is insufficient.
    """
    log(f"Scraping article from {url} using Playwright and Trafilatura")

    html_source = ""
    try:
        with sync_playwright() as p:
            browser = p.firefox.launch(
                headless=True,
                timeout=60000 # Overall browser launch timeout
            )
            page = browser.new_page(
                # Consider viewport size if it affects dynamic content loading
                # viewport={'width': 1280, 'height': 1024}
            )
            # Set overall navigation timeout and load timeout
            page.set_default_navigation_timeout(45000) # 45 seconds for navigation
            page.set_default_timeout(30000) # 30 seconds for other operations like page.content()
            
            log(f"Playwright: Navigating to {url}")
            # wait_until="networkidle" can be long; consider "load" or "domcontentloaded" if speed is an issue
            # and sites generally work. "networkidle" is more robust for SPAs.
            page.goto(url, wait_until="networkidle", timeout=45000) # Explicit timeout for goto
            log(f"Playwright: Page loaded. Waiting for network idle.")
            
            log(f"Playwright: Extracting page content for {url}")
            html_source = page.content()
            log(f"Playwright: Successfully fetched HTML source. Length: {len(html_source)}")
            browser.close()
    except PlaywrightTimeoutError as pte:
        log(f"FAIL {url}: Playwright navigation/load timeout: {pte}")
        # browser.close() might be needed here if error occurs after launch but before with statement closes
        return None
    except Exception as e_pw:
        log(f"FAIL {url}: Playwright failed: {e_pw}")
        log(traceback.format_exc())
        return None

    if not html_source:
        log(f"FAIL {url}: HTML source is empty after Playwright fetch.")
        return None

    try:
        log(f"Trafilatura: Extracting plain text and metadata from {url}")
        # Extract text and metadata first
        # include_tables=True to try and get table content if present
        # include_formatting=True might help with preserving some structure for text->markdown conversion later
        metadata_extraction_settings = {
            'output_format': 'json', # Crucial change: output as JSON when with_metadata=True
            'with_metadata': True, 
            'include_comments': False, 
            'include_tables': True,
            'include_formatting': True, # Might provide better structured plain text for Gemini
            'favor_recall': True # Try to get more content
        }
        json_string_data = trafilatura_extract(html_source, **metadata_extraction_settings)

        if not json_string_data:
            log(f"FAIL {url}: Trafilatura (metadata_extract with JSON) returned no data.")
            return None
        
        try:
            extracted_data = json.loads(json_string_data)
        except json.JSONDecodeError as e_json:
            log(f"FAIL {url}: Failed to parse JSON from Trafilatura: {e_json}. Data: {json_string_data[:500]}")
            return None

        if not extracted_data or not isinstance(extracted_data, dict) or 'text' not in extracted_data:
            log(f"FAIL {url}: Trafilatura JSON data is empty or not in expected dict format or missing 'text'. Data: {extracted_data}")
            return None

        plain_text = extracted_data.get('text', "")
        title = extracted_data.get('title') or "Untitled Article"
        # Other potential metadata: extracted_data.get('date'), extracted_data.get('author')
        log(f"Trafilatura: Extracted title: '{title}'. Plain text length: {len(plain_text)}")
        log(f"Trafilatura: First 200 chars of plain text: {plain_text[:200]}")

        if len(plain_text) < MIN_CONTENT_LENGTH:
            log(f"FAIL {url}: Trafilatura extracted text too short (length: {len(plain_text)}, min: {MIN_CONTENT_LENGTH}).")
            return None

        log(f"Trafilatura: Extracting cleaned HTML from {url}")
        # Extract cleaned HTML for PDF fallback (if Gemini Markdown fails)
        # We want to preserve as much structure as possible, so don't be too aggressive with includes/excludes
        html_extraction_settings = {
            'output_format': 'html',
            'include_comments': False,
            'include_tables': True,
            'include_formatting': True, # Retain some structural/formatting tags
            'favor_recall': True
        }
        cleaned_html_content = trafilatura_extract(html_source, **html_extraction_settings)

        if not cleaned_html_content:
            log(f"WARN {url}: Trafilatura (html_extract) returned no cleaned HTML. Plain text will still be used for Gemini.")
            # If cleaned_html_content is None, the fallback in main might be very basic.
            # We can construct a simple HTML from plain_text as a better fallback here.
            cleaned_html_content = f"<h1>{title}</h1><div>{plain_text.replace(chr(10), '<br />')}</div>"
            log(f"Using constructed HTML from plain text as fallback cleaned_html_content for {url}.")
        else:
            log(f"Trafilatura: Successfully extracted cleaned HTML. Length: {len(cleaned_html_content)}")

        log(f"Successfully scraped and extracted from {url}")
        return {
            'title': title,
            'html_content': cleaned_html_content, # This is the cleaned HTML from Trafilatura
            'plain_text': plain_text
        }

    except Exception as e_tf:
        log(f"FAIL {url}: Trafilatura extraction failed: {e_tf}")
        log(traceback.format_exc())
        return None


def html2pdf(final_html_content, out_path):
    """
    Convert final HTML content (fully styled) to PDF using WeasyPrint.
    """
    if not final_html_content:
        log("Cannot generate PDF: HTML content is empty")
        return False

    try:
        log(f"Generating PDF: {out_path}") # Font size info now part of final_html_content
        HTML(string=final_html_content).write_pdf(out_path)
        log(f"PDF generated successfully: {out_path}")
        return True
    except Exception as e:
        log(f"Failed to generate PDF: {e}")
        return False


def _sanitize_title_for_filename(title, max_length=50):
    """
    Sanitize the title for use in a filename.
    Replaces spaces with underscores, removes non-alphanumeric characters (except _ and -),
    and truncates to max_length.
    """
    if not title:
        return "untitled"
    # Keep alphanumeric, underscore, hyphen, space. Replace space with underscore later.
    sanitized = re.sub(r'[^\w\s-]', '', title.strip())
    sanitized = re.sub(r'\s+', '_', sanitized)
    return sanitized[:max_length]


def get_pdf_filename(rank, article_title, source_feed_title):
    """
    Generate PDF filename: YYYY-MM-DD_<sanitized_source_feed_title>_<rank>_<sanitized_article_title>.pdf
    """
    today = date.today().strftime("%Y-%m-%d")
    sanitized_article_slug = _sanitize_title_for_filename(article_title)
    sanitized_source_slug = _sanitize_title_for_filename(source_feed_title, max_length=25) # Shorter for source
    return f"{today}_{sanitized_source_slug}_{rank}_{sanitized_article_slug}.pdf"


def upload_to_supernote(pdf_files):
    """
    Upload PDF files to the specified path on Supernote using sncloud.
    Returns the number of successfully uploaded files.
    """
    test_mode_val = os.getenv("TEST_MODE", "False").lower()
    test_mode = test_mode_val in ("true", "1", "t", "yes")
    if test_mode:
        log("TEST MODE: Skipping actual upload to Supernote")
        files_str = f"{len(pdf_files)} files"
        if pdf_files:
            files_str += f": {', '.join(pdf_files)}"
        log(f"Would have uploaded {files_str}")
        return len(pdf_files)

    email = os.getenv("SUPERNOTE_EMAIL")
    password = os.getenv("SUPERNOTE_PASSWORD")
    target_path_str = os.getenv("SUPERNOTE_TARGET_PATH", "/Inbox/HackerNews")

    if not email or not password:
        log("ERROR: Supernote credentials not found in .env file")
        return 0

    if not target_path_str.startswith("/"):
        target_path_str = "/" + target_path_str
        log(f"Corrected Supernote target path to: {target_path_str}")
    
    target_folder_name = os.path.basename(target_path_str)
    parent_path_str = os.path.dirname(target_path_str)


    try:
        client = SNClient()
        log(f"Logging in to Supernote cloud with email: {email}")
        client.login(email, password)
        log("Successfully logged in to Supernote cloud")

        path_exists = False
        try:
            current_path_items = client.ls(directory=parent_path_str)
            for item in current_path_items:
                log(f"Item: {item}")
                if item.file_name == target_folder_name and item.is_folder:
                    path_exists = True
                    log(f"Found target folder: {target_path_str}")
                    break
            
            if not path_exists:
                log(f"Target folder '{target_folder_name}' not found in '{parent_path_str}'. Attempting to create it.")
                client.mkdir(target_folder_name, parent_path=parent_path_str)
                log(f"Successfully created folder: {target_path_str}")
                path_exists = True

        except Exception as e:
            log(f"Error while checking or creating target folder '{target_path_str}': {e}")
            log(f"Please ensure the base path '{parent_path_str}' exists or create '{target_path_str}' manually.")
            return 0

        if not path_exists:
             log(f"ERROR: Target folder '{target_path_str}' could not be found or created.")
             return 0

        uploaded_count = 0
        for pdf_file in pdf_files:
            try:
                log(f"Uploading {pdf_file} to Supernote path '{target_path_str}'...")
                client.put(file_path=Path(pdf_file), parent=target_path_str)
                log(f"Successfully uploaded {pdf_file}")
                uploaded_count += 1
            except Exception as e:
                log(f"ERROR uploading {pdf_file}: {e}")
                log(traceback.format_exc())


        return uploaded_count

    except Exception as e:
        log(f"ERROR in Supernote upload process: {e}")
        log(traceback.format_exc())
        return 0


def classify_article_quality(article_text):
    """
    Classifies article quality using Gemini API.
    Returns True if thought-provoking, False if advertisement/low-quality.
    Defaults to True if API key is missing or an error occurs.
    """
    api_key = os.getenv("GEMINI_API_KEY")
    if not api_key:
        log("GEMINI_API_KEY not found in .env. Skipping AI classification, defaulting to 'good'.")
        return True

    try:
        # Ensure google.generativeai is imported and configure if necessary
        # genai.configure(api_key=api_key) # Configure is typically done once
        # For simplicity, let's assume it's configured if API key is present or handle client instantiation

        model = genai.GenerativeModel('gemini-2.5-flash-preview-04-17') 

        prompt = (
            "You are an expert content quality analyst. Your task is to classify an article based on its content. "
            "Determine if the article is a substantive, thought-provoking piece that offers insights, analysis, or in-depth information. "
            "Alternatively, determine if it is primarily an advertisement, promotional material, very superficial content, a product announcement without deeper substance, or a low-quality piece. "
            "Respond with only one of these two words: 'thought-provoking' or 'advertisement'.\n\n"
            "Article Content (first 10000 characters):\n"
            f"{article_text[:10000]}" # Truncate to a reasonable length
        )

        response = model.generate_content(prompt)
        
        # Robustly check for candidates and parts
        if not response.candidates:
            log(f"WARN: Gemini (classify) returned no candidates. Prompt feedback: {response.prompt_feedback if hasattr(response, 'prompt_feedback') else 'N/A'}")
            return True # Default to good if no candidates
        
        # Assuming we care about the first candidate
        candidate = response.candidates[0]
        if not candidate.content or not candidate.content.parts:
            log(f"WARN: Gemini (classify) first candidate has no content/parts. Finish reason: {candidate.finish_reason if hasattr(candidate, 'finish_reason') else 'N/A'}. Safety ratings: {candidate.safety_ratings if hasattr(candidate, 'safety_ratings') else 'N/A'}")
            return True # Default to good

        response_text = candidate.content.parts[0].text.strip().lower()
        log(f"Gemini classification raw response: '{response_text}'")

        if "thought-provoking" in response_text:
            return True
        elif "advertisement" in response_text:
            return False
        else:
            log(f"Warning: Gemini classification response was not definitive: '{response.text}'. Defaulting to 'good'.")
            return True # Default to true if unsure

    except ImportError:
        log("ERROR: google-generativeai library not installed or not found. Skipping AI classification.")
        return True 
    except Exception as e:
        log(f"ERROR during Gemini API call: {e}")
        log(traceback.format_exc())
        return True # Default to true in case of API error


def reformat_to_markdown_gemini(article_text, article_url, article_publish_date):
    """
    Reformats article text to Markdown using Gemini API, adds date/URL near top, and appends source URL.
    Returns Markdown string or None if an error occurs or API key is missing.
    """
    api_key = os.getenv("GEMINI_API_KEY")
    if not api_key:
        log("GEMINI_API_KEY not found in .env. Skipping Gemini Markdown reformatting.")
        return None
    
    if not article_text or not article_text.strip():
        log("Article text is empty. Skipping Gemini Markdown reformatting.")
        return None

    try:
        model = genai.GenerativeModel('gemini-1.5-flash') # Assumes genai.configure was called
        
        # Construct the prompt as a list of parts: instructions and then the article text
        instructional_prompt = (
            "You are an expert text reformatter. Your task is to convert the following article content into clean, readable, and well-structured Markdown. "
            "Focus on preserving the original meaning and structure (headings, paragraphs, lists, blockquotes, code blocks if any) as much as possible. "
            "Do not add any commentary, preamble, or explanation of your own. Only output the Markdown. "
            "Ensure that the Markdown is suitable for direct conversion to HTML and then to PDF.\n\n"
            "Make sure to remove any obvious ads or promotional material. "
            "Make sure to generate a heading for the article based on the content. "
            # The article content itself will be passed as a separate part below
        )

        contents_for_gemini = [
            instructional_prompt, 
            article_text # Pass the full article text here
        ]

        log(f"Sending article of length {len(article_text)} to Gemini for Markdown reformatting.")

        response = model.generate_content(
            contents_for_gemini, # Pass the list of contents
            generation_config=genai.types.GenerationConfig(
                # Consider adding temperature, top_p, top_k if needed for creativity vs. fidelity
                # max_output_tokens= can be set if there's a strict limit desired for the output.
            )
        )
        
        # Robustly check for candidates and parts
        if not response.candidates:
            log(f"WARN: Gemini (reformat) returned no candidates. Prompt feedback: {response.prompt_feedback if hasattr(response, 'prompt_feedback') else 'N/A'}")
            return None # Cannot reformat if no candidates
        
        candidate = response.candidates[0]
        if not candidate.content or not candidate.content.parts:
            log(f"WARN: Gemini (reformat) first candidate has no content/parts. Finish reason: {candidate.finish_reason if hasattr(candidate, 'finish_reason') else 'N/A'}. Safety ratings: {candidate.safety_ratings if hasattr(candidate, 'safety_ratings') else 'N/A'}")
            return None # Cannot reformat

        markdown_output = candidate.content.parts[0].text.strip()
        log(f"Gemini Markdown reformatting successful. Output length: {len(markdown_output)}")
        if not markdown_output:
            log("Gemini returned empty Markdown. Treating as failure.")
            return None
        
        # Prepare date string (e.g., YYYY-MM-DD)
        date_str = "Date N/A"
        if article_publish_date:
            try:
                date_str = article_publish_date.strftime("%Y-%m-%d")
            except AttributeError: # Handle if it's not a datetime object for some reason
                log(f"WARN: article_publish_date '{article_publish_date}' is not a datetime object, cannot format.")
                date_str = str(article_publish_date) # Use as is if not datetime

        # Attempt to insert URL and Date after the first heading, or at the beginning
        lines = markdown_output.split('\n')
        inserted_metadata = False
        if lines and lines[0].strip().startswith("#"):
            # First line is a heading, insert after it
            metadata_line = f"\n[{article_url}]({article_url}) - Published: {date_str}\n"
            lines.insert(1, metadata_line)
            markdown_output = "\n".join(lines)
            inserted_metadata = True
            log(f"Inserted metadata after first heading. Markdown length: {len(markdown_output)}")
        else:
            # No clear heading at the start, prepend metadata
            metadata_line = f"[{article_url}]({article_url}) - Published: {date_str}\n\n"
            markdown_output = metadata_line + markdown_output
            inserted_metadata = True # Even if at top, it's placed
            log(f"Prepended metadata as no initial heading found. Markdown length: {len(markdown_output)}")

        # Append the source URL at the very end (this might be redundant if already at top, but good for consistency)
        markdown_output += f"\n\n---\nOriginal article: [{article_url}]({article_url}) (repeated for clarity)"
        log(f"Appended source URL to Gemini Markdown. Total length: {len(markdown_output)}")

        return markdown_output

    except ImportError:
        log("ERROR: google-generativeai library not installed or not found. Skipping Gemini Markdown reformatting.")
        return None
    except Exception as e:
        log(f"ERROR during Gemini API call for Markdown reformatting: {e}")
        log(traceback.format_exc())
        return None


def convert_markdown_to_styled_html(markdown_string, font_size):
    """
    Converts Markdown string to a full HTML document with embedded font style.
    """
    if not markdown_string:
        return ""
    
    html_fragment = markdown2.markdown(markdown_string, extras=["fenced-code-blocks", "cuddled-lists", "tables", "strike"])
    
    # Basic CSS for Markdown elements - can be expanded
    # Ensures code blocks and preformatted text wrap correctly.
    # Adds some basic styling for tables.
    markdown_css = """
        body { 
            font-family: sans-serif; /* Or your preferred font */
            line-height: 1.6;
            font-size: """ + font_size + """;
        }
        h1, h2, h3, h4, h5, h6 { 
            margin-top: 1.5em; 
            margin-bottom: 0.5em; 
            line-height: 1.3;
        }
        p { 
            margin-bottom: 1em; 
        }
        ul, ol { 
            margin-bottom: 1em; 
            padding-left: 2em;
        }
        li { 
            margin-bottom: 0.3em; 
        }
        blockquote { 
            margin-left: 2em; 
            padding-left: 1em; 
            border-left: 3px solid #eee; 
            color: #555;
        }
        pre { 
            background-color: #f5f5f5; 
            padding: 1em; 
            overflow-x: auto; /* Allows horizontal scrolling for wide code */
            white-space: pre-wrap;       /* CSS3 */
            white-space: -moz-pre-wrap;  /* Mozilla, since 1999 */
            white-space: -pre-wrap;      /* Opera 4-6 */
            white-space: -o-pre-wrap;    /* Opera 7 */
            word-wrap: break-word;       /* Internet Explorer 5.5+ */
        }
        code { 
            font-family: monospace; 
            background-color: #f5f5f5; 
            padding: 0.2em 0.4em; 
            border-radius: 3px;
        }
        pre code { /* Reset padding for code inside pre as pre has its own */
            padding: 0;
            background-color: transparent;
            border-radius: 0;
        }
        table {
            border-collapse: collapse;
            width: 100%;
            margin-bottom: 1em;
        }
        th, td {
            border: 1px solid #ddd;
            padding: 8px;
            text-align: left;
        }
        th {
            background-color: #f2f2f2;
        }
        img {
            max-width: 100%;
            height: auto;
        }
    """
    
    styled_html_document = f"""
    <!DOCTYPE html>
    <html lang="en">
        <head>
            <meta charset="UTF-8">
            <title>Generated PDF Content</title>
            <style>
                {markdown_css}
            </style>
        </head>
        <body>
            {html_fragment}
        </body>
    </html>
    """
    return styled_html_document


def main():
    """
    Main function to orchestrate the pipeline:
    1. Fetch top HN links
    2. Scrape articles
    3. Generate PDFs
    4. Upload PDFs to Supernote
    """
    log("Starting HN to Supernote pipeline")

    # Initialize summary counters (MOVED BEFORE MAIN TRY BLOCK)
    links_fetched_count = 0
    scrape_success_count = 0
    scrape_failed_count = 0
    classified_good_count = 0
    classified_bad_or_short_count = 0 
    pdf_generated_count = 0
    pdf_generation_failed_count = 0
    upload_attempted_count = 0
    upload_successful_count = 0
    articles_skipped_from_history_count = 0 # Counter for summary

    # Initialize lists for detailed error reporting (MOVED BEFORE MAIN TRY BLOCK)
    articles_failed_scrape_details = []
    articles_skipped_post_scrape_details = [] 
    articles_failed_pdf_generation_details = []
    pdfs_failed_upload_details = [] 

    try:
        articles_to_process, articles_skipped_from_history_in_fetch = get_articles_from_opml()
        articles_skipped_from_history_count = articles_skipped_from_history_in_fetch # Store for final summary

        if not articles_to_process:
            log("No new articles found from OPML feeds (after checking history). Exiting.")
            # Log summary before exiting early
            log("--- Pipeline Summary ---")
            log(f"Articles fetched from OPML (new for this run): {links_fetched_count}")
            log(f"Articles skipped from history: {articles_skipped_from_history_count}")
            # ... other zero counts ...
            log("------------------------")
            return
        
        links_fetched_count = len(articles_to_process) # This is now count of *new* articles for this run
        pdf_files = []
        processed_article_count_in_run = 0 # For rank in filename

        for article_info in articles_to_process:
            processed_article_count_in_run += 1
            current_rank = processed_article_count_in_run
            link = article_info['link']
            source_feed_title = article_info['source_feed_title']
            publish_date = article_info.get('publish_date') # Get the publish_date
            # article_title_from_feed = article_info['article_title_from_feed'] # Available if needed

            try:
                log(f"Processing article {current_rank}/{links_fetched_count}: '{link}' from feed '{source_feed_title}'")
                article_data = scrape(link) # scrape still returns title from actual scraping

                if article_data:
                    scrape_success_count += 1
                    # Use the title from scraping as it's usually more accurate/complete
                    scraped_article_title = article_data['title'] 
                    original_html_content = article_data['html_content']
                    plain_text = article_data.get('plain_text', "")

                    if not plain_text.strip() or len(plain_text.strip()) < MIN_CONTENT_LENGTH:
                        reason = f"Plain text empty or too short ({len(plain_text.strip())} chars, min: {MIN_CONTENT_LENGTH})"
                        log(f"WARN: For '{scraped_article_title}' ({link}): {reason}. Skipping.")
                        articles_skipped_post_scrape_details.append({'link': link, 'title': scraped_article_title, 'source_feed': source_feed_title, 'reason': reason})
                        classified_bad_or_short_count +=1
                    else:
                        is_good_article = classify_article_quality(plain_text)
                        log(f"Article '{scraped_article_title}' classified as {'good' if is_good_article else 'not good/advertisement'}.")

                        if is_good_article:
                            classified_good_count += 1
                            pdf_name = get_pdf_filename(current_rank, scraped_article_title, source_feed_title)
                            final_html_for_pdf = None
                            pdf_font_size = os.getenv("PDF_FONT_SIZE", "14pt")

                            gemini_markdown = reformat_to_markdown_gemini(plain_text, link, publish_date)

                            if gemini_markdown:
                                log(f"Successfully reformatted '{scraped_article_title}' to Markdown. Converting to HTML.")
                                final_html_for_pdf = convert_markdown_to_styled_html(gemini_markdown, pdf_font_size)
                                log(f"Converted Gemini Markdown to styled HTML for '{scraped_article_title}'.")
                            else:
                                log(f"Failed to reformat '{scraped_article_title}' to Markdown. Falling back to original scraped HTML.")
                                style_tag = f"<style>body {{ font-size: {pdf_font_size}; }}</style>"
                                final_html_for_pdf = style_tag + original_html_content
                                log(f"Using original scraped HTML for '{scraped_article_title}'.")
                            
                            if final_html_for_pdf:
                                if html2pdf(final_html_for_pdf, pdf_name):
                                    pdf_files.append(pdf_name)
                                    pdf_generated_count += 1
                                    log(f"Successfully processed '{scraped_article_title}' as PDF: {pdf_name}")
                                    # Append to history file immediately after successful PDF generation
                                    try:
                                        with open(HISTORY_FILE, 'a') as hf:
                                            hf.write(link + "\n")
                                        log(f"Appended to history: {link}")
                                    except Exception as e_hist_write:
                                        log(f"WARN: Could not write to history file {HISTORY_FILE} for link {link}: {e_hist_write}")
                                else:
                                    pdf_generation_failed_count += 1
                                    reason = f"html2pdf returned false for {pdf_name}"
                                    articles_failed_pdf_generation_details.append({'link': link, 'title': scraped_article_title, 'source_feed': source_feed_title, 'reason': reason})
                                    log(f"Failed to generate PDF for '{scraped_article_title}' ({link}): {reason}")
                            else:
                                pdf_generation_failed_count += 1
                                reason = "No final HTML content available for PDF"
                                articles_failed_pdf_generation_details.append({'link': link, 'title': scraped_article_title, 'source_feed': source_feed_title, 'reason': reason})
                                log(f"{reason} for '{scraped_article_title}' ({link}).")
                        else:
                            classified_bad_or_short_count += 1
                            reason = "Classified as not good/advertisement"
                            articles_skipped_post_scrape_details.append({'link': link, 'title': scraped_article_title, 'source_feed': source_feed_title, 'reason': reason})
                            log(f"Skipping PDF for '{scraped_article_title}' ({link}): {reason}")
                else:
                    scrape_failed_count += 1
                    reason = "scrape(link) returned None or empty data"
                    articles_failed_scrape_details.append({'link': link, 'source_feed': source_feed_title, 'reason': reason})
                    log(f"FAIL {link} from '{source_feed_title}'. Reason: {reason}")
            except Exception as e:
                scrape_failed_count +=1
                error_message = str(e)
                articles_failed_scrape_details.append({'link': link, 'source_feed': source_feed_title, 'reason': error_message})
                log(f"FAIL processing link {link} from '{source_feed_title}': {error_message}")
                log(traceback.format_exc())

        log(f"Generated {pdf_generated_count} PDF files out of {classified_good_count} good articles considered.")

        if pdf_files:
            try:
                uploaded_count = upload_to_supernote(pdf_files)
                log(f"Uploaded {uploaded_count} of {len(pdf_files)} files")
            except Exception as e:
                log(f"FAIL during upload to Supernote: {e}")
                log(traceback.format_exc())
        else:
            log("No PDF files to upload")

        for pdf in pdf_files:
            print(f"Created: {pdf}")

        log(f"PDFs successfully uploaded: {upload_successful_count}")
        log(f"PDFs failed to upload: {upload_attempted_count - upload_successful_count}")
        log("------------------------")

        if articles_failed_scrape_details:
            log("--- Articles Failed to Scrape ---")
            for item in articles_failed_scrape_details:
                log(f"  Link: {item['link']}, Source: {item['source_feed']}, Reason: {item['reason']}")
        
        if articles_skipped_post_scrape_details:
            log("--- Articles Skipped After Successful Scrape ---")
            for item in articles_skipped_post_scrape_details:
                log(f"  Link: {item['link']}, Title: {item.get('title', 'N/A')}, Source: {item.get('source_feed', 'N/A')}, Reason: {item['reason']}")

        if articles_failed_pdf_generation_details:
            log("--- Articles Failed PDF Generation ---")
            for item in articles_failed_pdf_generation_details:
                log(f"  Link: {item['link']}, Title: {item.get('title', 'N/A')}, Source: {item.get('source_feed', 'N/A')}, Reason: {item['reason']}")
        
        # Placeholder for detailed upload failures if upload_to_supernote is modified
        if pdfs_failed_upload_details: # This list is not yet populated by current code
            log("--- PDFs Failed to Upload ---")
            for item in pdfs_failed_upload_details:
                log(f"  File: {item['pdf_filename']}, Reason: {item['reason']}")
        log("------------------------")

        # Log summary at the very end
        log("--- Pipeline Summary ---")
        log(f"Total unique articles from OPML feeds (new for this run): {links_fetched_count}")
        log(f"Articles skipped from history file: {articles_skipped_from_history_count}")
        log(f"Articles successfully scraped: {scrape_success_count}")
        log(f"Articles failed to scrape: {scrape_failed_count}")
        log(f"Good articles classified: {classified_good_count}")
        log(f"Bad or short articles: {classified_bad_or_short_count}")
        log(f"PDFs generated: {pdf_generated_count}")
        log(f"PDFs failed to generate: {pdf_generation_failed_count}")
        log(f"PDFs successfully uploaded: {upload_successful_count}")
        log(f"PDFs failed to upload: {upload_attempted_count - upload_successful_count}")
        log("------------------------")

    except Exception as e:
        log(f"CRITICAL ERROR in pipeline: {e}")
        log(traceback.format_exc())


if __name__ == "__main__":
    # It's good practice to configure API keys once, e.g., at the start if using genai.configure()
    # However, passing api_key to GenerativeModel constructor also works per call.
    # If GEMINI_API_KEY is in .env, load_dotenv() at the top should make it available via os.getenv()
    if os.getenv("GEMINI_API_KEY"):
        try:
            import google.generativeai as genai
            genai.configure(api_key=os.getenv("GEMINI_API_KEY"))
            log("Gemini API configured successfully.")
        except ImportError:
            log("Attempted to configure Gemini API, but google-generativeai library is not installed.")
        except Exception as e:
            log(f"Error configuring Gemini API: {e}")
            
    main()
