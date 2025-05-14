import feedparser
import os
import logging
import traceback
from datetime import date
from dotenv import load_dotenv
from newspaper import Article
from weasyprint import HTML
from sncloud import SNClient
from pathlib import Path

load_dotenv()

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(levelname)s - %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)


def log(message):
    """Log a message to both console and log file"""
    logging.info(message)


COUNT = int(os.getenv("HN_ITEMS", 10))
MIN_POINTS = int(os.getenv("HN_MIN_POINTS", 30))


def top_links():
    """
    Fetch top Hacker News stories with minimum points
    Returns a list of URLs for the top stories
    """
    url = f"https://hnrss.org/frontpage?points={MIN_POINTS}"
    log(f"Fetching RSS feed from {url}")

    try:
        feed = feedparser.parse(url)
        links = [e.link for e in feed.entries[:COUNT]]
        log(f"Successfully fetched {len(links)} links")
        return links
    except Exception as e:
        log(f"ERROR fetching RSS feed: {e}")
        return []


def scrape(url):
    """
    Extract article text using newspaper3k
    Falls back to readability-lxml if newspaper3k fails
    Returns HTML content with title and text
    """
    log(f"Scraping article from {url}")

    try:
        art = Article(url)
        art.download()
        art.parse()

        if not art.text or len(art.text) < 100:
            raise ValueError("Article text too short, might be paywalled")

        log(f"Successfully scraped article: {art.title}")
        return f"<h1>{art.title}</h1>{art.text.replace(chr(10), '<br>')}"

    except Exception as e:
        log(f"newspaper3k failed for {url}: {e}")
        log("Trying with readability-lxml as fallback")

        try:
            import requests
            from readability import Document

            response = requests.get(url, timeout=10)
            doc = Document(response.content)
            title = doc.title()
            content = doc.summary()

            if not content or len(content) < 100:
                log(f"FAIL {url}: Content too short, might be paywalled")
                return None

            log(f"Successfully scraped article with fallback: {title}")
            return f"<h1>{title}</h1>{content}"

        except Exception as e2:
            log(f"FAIL {url}: Both scrapers failed: {e2}")
            return None


def html2pdf(html, out_path):
    """
    Convert HTML content to PDF using WeasyPrint
    """
    if not html:
        log("Cannot generate PDF: HTML content is empty")
        return False

    try:
        log(f"Generating PDF: {out_path}")
        HTML(string=html).write_pdf(out_path)
        log(f"PDF generated successfully: {out_path}")
        return True
    except Exception as e:
        log(f"Failed to generate PDF: {e}")
        return False


def get_pdf_filename(rank):
    """
    Generate PDF filename following the convention: YYYY-MM-DD_HN_<rank>.pdf
    """
    today = date.today().strftime("%Y-%m-%d")
    return f"{today}_HN_{rank}.pdf"


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


def main():
    """
    Main function to orchestrate the HN to Supernote pipeline:
    1. Fetch top HN links
    2. Scrape articles
    3. Generate PDFs
    4. Upload PDFs to Supernote
    """
    log("Starting HN to Supernote pipeline")

    try:
        links = top_links()
        if not links:
            log("No links found. Exiting.")
            return

        pdf_files = []

        for rank, link in enumerate(links, start=1):
            try:
                log(f"Processing article {rank}: {link}")
                html = scrape(link)

                if html:
                    pdf_name = get_pdf_filename(rank)
                    if html2pdf(html, pdf_name):
                        pdf_files.append(pdf_name)
                        log(f"Successfully processed article {rank}")
                else:
                    log(f"FAIL {link}: Unable to scrape content")
            except Exception as e:
                log(f"FAIL {link}: {e}")
                log(traceback.format_exc())

        log(f"Generated {len(pdf_files)} PDF files")

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

    except Exception as e:
        log(f"CRITICAL ERROR in pipeline: {e}")
        log(traceback.format_exc())


if __name__ == "__main__":
    main()
