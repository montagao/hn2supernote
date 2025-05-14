# Hacker News to Supernote

This project fetches top articles from Hacker News, converts them to PDF, and uploads them to your Supernote device.

## Features

*   Fetches top Hacker News stories based on a minimum point threshold.
*   Scrapes article content using `newspaper3k` with a fallback to `readability-lxml`.
*   Converts articles to PDF format.
*   Uploads PDFs to a specified folder (default: "HackerNews") on your Supernote cloud account.
*   Configurable via a `.env` file.
*   Includes a test mode to simulate uploads without actually connecting to the Supernote cloud.

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

3.  **Create a `.env` file:**
    Copy the example below and create a `.env` file in the root of the project directory.
    ```dotenv
    # Hacker News settings
    HN_ITEMS=10 # Number of articles to fetch
    HN_MIN_POINTS=30 # Minimum points for an article to be considered

    # Supernote Credentials
    SUPERNOTE_EMAIL="your_supernote_email@example.com"
    SUPERNOTE_PASSWORD="your_supernote_password"

    # Supernote Target Path (Required)
    # The full path to the target folder on your Supernote device where PDFs will be uploaded.
    # Examples: "/HackerNews", "/Notes/HackerNews", "/My Reading List"
    # The script will attempt to create this folder if it doesn't exist.
    # Make sure the base path (e.g., "/Notes" if target is "/Notes/HackerNews") exists.
    SUPERNOTE_TARGET_PATH="/HackerNews"

    # Test Mode (Optional)
    # Set to "True" to skip actual Supernote uploads (useful for testing scraping and PDF generation)
    # TEST_MODE=False
    ```
    **Important:**
    *   Replace placeholder values with your actual credentials and preferences.
    *   Set `SUPERNOTE_TARGET_PATH` to the desired folder path on your Supernote. The script will try to create the final folder in the path if it doesn't exist (e.g., "HackerNews" in "/Notes/HackerNews"), but the base directory (e.g., "/Notes") must exist.

## Usage

Run the main script from the project's root directory:

```bash
python hn2sn.py
```

The script will:
1.  Fetch links from Hacker News.
2.  Scrape the content for each link.
3.  Generate a PDF for each successfully scraped article. The PDFs will be named in the format `YYYY-MM-DD_HN_<rank>.pdf` and saved in the current directory.
4.  Upload the generated PDFs to your Supernote cloud.

Logs are printed to the console and provide information about the process, including any errors.

## Supernote API Client

This project uses the `sncloud` library ([https://github.com/julianprester/sncloud](https://github.com/julianprester/sncloud)) to interact with the Supernote Cloud API.

## Contributing

Feel free to open issues or submit pull requests if you have suggestions for improvements or bug fixes. 