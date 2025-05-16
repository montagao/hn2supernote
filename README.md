# Hacker News to Supernote

This project fetches articles from RSS feeds defined in an OPML file, optionally classifies them for quality and reformats them using AI, converts them to PDF, and uploads them to your Supernote device. It keeps track of processed articles to avoid duplicates across runs.

## Features

*   **OPML Feed Input**: Parses a user-provided OPML file to fetch articles from multiple RSS feeds.
*   **Configurable Fetching**: 
    *   Limits the number of articles fetched per feed in each run (`MAX_ITEMS_PER_FEED`).
    *   Optional global limit on the total number of new articles processed per run (`MAX_TOTAL_ARTICLES`).
*   **Persistent History**: Remembers successfully processed articles in a history file (`processed_articles.log` by default) to avoid reprocessing in subsequent runs.
*   **Advanced Scraping**: Uses `Playwright` to render JavaScript-heavy pages and `trafilatura` for robust article text and metadata extraction.
*   **AI-Powered Content Processing (Optional, requires Gemini API Key)**:
    *   Classifies articles as "thought-provoking" or "advertisement/low-quality" using the Gemini API.
    *   Reformats article content to clean Markdown using the Gemini API for potentially better PDF structure and readability.
*   Converts articles to PDF format, with configurable font size.
*   Uploads PDFs to a specified folder on your Supernote cloud account.
*   Configurable via a `.env` file.
*   Includes a test mode to simulate uploads.
*   Detailed summary log at the end of each run.

## Setup

1.  **Clone the repository:**
    ```bash
    git clone <repository-url>
    cd <repository-directory>
    ```

2.  **Install dependencies:**
    Make sure you have Python 3 installed.
    ```bash
    pip install -r requirements.txt
    ```

3.  **Install Playwright browser binaries:**
    The script uses Playwright with Firefox by default.
    ```bash
    playwright install firefox
    ```

4.  **Prepare your OPML file:**
    Create an OPML file (e.g., `feeds.opml`) listing your desired RSS feeds. See example format in user queries.

5.  **Create a `.env` file:**
    Copy the example below and create a `.env` file in the root of the project directory.
    ```dotenv
    # OPML and Feed Settings
    OPML_FILE_PATH="feeds.opml" # Path to your OPML file
    MAX_ITEMS_PER_FEED=5      # Max new articles to fetch from each feed per run
    MAX_TOTAL_ARTICLES=20     # Optional: Max total new articles to process in a single run (0 or not set for no limit)
    HISTORY_FILE="processed_articles.log" # File to store links of processed articles (optional, defaults to this name)

    # Supernote Credentials
    SUPERNOTE_EMAIL="your_supernote_email@example.com"
    SUPERNOTE_PASSWORD="your_supernote_password"
    SUPERNOTE_TARGET_PATH="/Inbox/Feeds" # Target folder on Supernote

    # PDF Formatting (Optional)
    PDF_FONT_SIZE="16pt" # Default is 16pt. Examples: "12pt", "1.2em"

    # Gemini API Key (Optional for AI features)
    # GEMINI_API_KEY="YOUR_GEMINI_API_KEY"

    # Test Mode (Optional)
    # TEST_MODE=False
    ```
    **Important:**
    *   Set `OPML_FILE_PATH` to the path of your OPML file.
    *   Adjust `MAX_ITEMS_PER_FEED` and `MAX_TOTAL_ARTICLES` as needed.
    *   Replace Supernote placeholder values with your actual credentials and desired target path.
    *   If you want to use the AI features, uncomment and set your `GEMINI_API_KEY`.

## Usage

Run the main script from the project's root directory:

```bash
python hn2sn.py
```

The script will:
1.  Read your OPML file.
2.  Fetch new articles from each feed (respecting `MAX_ITEMS_PER_FEED` and `MAX_TOTAL_ARTICLES`), skipping those already in `HISTORY_FILE`.
3.  For each new article:
    a.  Scrape the content using Playwright and Trafilatura.
    b.  (If GEMINI_API_KEY is set) Classify article quality.
    c.  (If GEMINI_API_KEY is set and article is good) Attempt to reformat to Markdown via Gemini.
    d.  Generate a PDF. If Markdown reformatting was successful, it's used; otherwise, the cleaned HTML from scraping is used.
    e.  PDFs are named `YYYY-MM-DD_<sanitized_source_feed_title>_<rank>_<sanitized_article_title>.pdf` and saved locally.
    f.  Successfully generated article links are added to `HISTORY_FILE`.
4.  Upload the generated PDFs to your Supernote cloud (unless in Test Mode).
5.  Print a detailed summary of the run.

## Supernote API Client

This project uses the `sncloud` library ([https://github.com/julianprester/sncloud](https://github.com/julianprester/sncloud)) to interact with the Supernote Cloud API.

## Contributing

Feel free to open issues or submit pull requests if you have suggestions for improvements or bug fixes. 
## License

This project is licensed under the [MIT License](LICENSE).

