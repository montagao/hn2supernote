# SendToSupernote TODO List

## FastAPI Backend

### Core Functionality
- [ ] **Setup FastAPI Application**:
    - [x] Basic app structure (`main.py`).
    - [x] Dependency management (e.g., `requirements.txt` specific to the backend or integrated into the main one).
- [ ] **API Endpoint (`/api/queue_article`)**:
    - [x] Define request body (URL, HTML content, user credentials - email/password for Supernote). (Pydantic models `ArticleQueueRequest` and `ArticleQueueResponse` created in `main.py`)
    - [x] Basic validation of incoming data. (Pydantic validators added for `target_path`, `pdf_font_size`, `html_content`)
- [ ] **Authentication/Authorization**:
    - [ ] Initial simple check for `email` and `password` (e.g., against environment variables or a config file).
    - [ ] Consider more robust auth for future.
- [x] **API Endpoint (`/api/queue_article`)**: Models updated for token auth.
- [ ] **Authentication/Authorization (Token-based)**:
    - [ ] **New Endpoint (`/api/auth/login`)**: Created with `LoginRequest` and `TokenResponse` models. (Initial version in `main.py`)
    - [ ] **Token Generation & Storage**: Simple UUID token generated and stored in-memory (in `main.py`). *Needs hardening for production.*
    - [x] **Credential Verification**: Placeholder `verify_supernote_credentials` created. Needs real SNClient check. (Implemented in `main.py`)
    - [ ] **Token-based Protection for `/api/queue_article`**: Initial `Depends(OAuth2PasswordBearer(...))` added. *Needs refinement for Bearer token header.*
    - [x] Create custom FastAPI dependency for Bearer token validation. (`get_validated_user_info_from_token` in `main.py`)
- [ ] **Article Processing Pipeline (Port from `hn2sn.py`)**:
    - [x] **Modularize `hn2sn.py`**: Refactor functions from `hn2sn.py` (scraping, classification, Markdown reformatting, PDF generation, Supernote upload) to be easily importable and usable by the FastAPI app. (All core functions moved to `processing.py`)
    - [x] **Scraping**: Adapt `scrape()` function. Consider how to handle Playwright (sync vs async in FastAPI context). (Moved to `processing.py` as `scrape_article_content`)
    - [ ] **Content Extraction**: Ensure `Readability.js` content passed from extension is handled, or decide if backend should re-fetch/re-extract. The current extension sends extracted HTML, so the backend might use that directly with Trafilatura or just for Gemini.
    - [x] **AI Classification**: Adapt `classify_article_quality()` for use in the backend. (Moved to `processing.py`)
    - [x] **AI Markdown Reformatting**: Adapt `reformat_to_markdown_gemini()`. (Moved to `processing.py`)
    - [x] **PDF Generation**: Adapt `html2pdf()` and `convert_markdown_to_styled_html()`. Font size might need to come from extension settings or backend config. (Moved to `processing.py` as `generate_pdf_from_html` and `convert_markdown_to_styled_html`)
    - [x] **Supernote Upload**: Adapt `upload_to_supernote()`. Securely handle credentials passed from the extension. (Moved to `processing.py` as `upload_pdfs_to_supernote` and helpers)
- [x] **Background Task/Queueing**:
    - [x] Implement a simple background task runner (e.g., FastAPI's `BackgroundTasks`) for the article processing pipeline to avoid blocking API responses. (Basic structure added to `/api/queue_article`, full pipeline implemented in `process_article_in_background`)
    - [ ] Consider a more robust queue (e.g., Celery with Redis/RabbitMQ) for scalability if needed later.
- [x] **Configuration Management**:
    - [x] Use environment variables (e.g., via `.env` file) for Supernote credentials (if not passed per request), Gemini API key, PDF settings. (Gemini API key config on startup added)
- [x] **Logging**:
    - [x] Implement comprehensive logging for the backend operations. (Basic logging setup in `main.py` and used in `processing.py`)
- [ ] **Error Handling**:
    - [ ] Robust error handling and appropriate HTTP responses for the API.

### Enhancements & Future Work
- [ ] **Status Endpoint**: API endpoint to check the status of a queued article.
- [ ] **Persistent History**: Decide if the backend should maintain its own processed articles history, or if the extension's implicit history (not re-sending same tab) is enough.
- [ ] **User Management**: If scaling, consider proper user accounts instead of passing credentials each time.

## Chrome Extension

### Core Functionality (Review & Refine)
- [ ] **`popup.js`**:
    - [ ] Review logic for sending data to backend.
    - [ ] Improve user feedback during processing (e.g., "Sending...", "Processing...").
- [ ] **`content.js`**:
    - [ ] Review `Readability.js` integration. Ensure it handles various site structures well.
    - [ ] Error handling if extraction fails.
- [ ] **`options.js` / `options.html`**:
    - [ ] Ensure saving and loading of backend URL, email, and password works reliably.
    - [ ] Add validation for input fields (e.g., backend URL format).
- [ ] **`manifest.json`**:
    - [ ] Add other standard icon sizes (16, 32, 48) to `icons` field.
    - [ ] Review permissions.
- [ ] **Error Handling**:
    - [ ] More comprehensive error display to the user in `popup.html` if backend request fails or returns an error.

### Enhancements & Future Work
- [ ] **Dark Mode Support**: For popup and options page.
- [ ] **Configuration Sync**: Sync settings across browsers if user is signed into Chrome.
- [ ] **Option to send selected text**: Instead of the whole article.
- [ ] **Advanced Options**:
    - [ ] Allow user to specify Supernote target path via extension options.
    - [ ] Allow user to configure PDF font size via extension options.

## General Project
- [ ] **Testing**:
    - [ ] Unit tests for backend modules.
    - [ ] Integration tests for the API.
    - [ ] Basic manual testing for the Chrome extension.
- [ ] **Documentation**:
    - [ ] Update main `README.md` for `hn2supernote` project to include setup and usage for the `sendtosupernote` extension and backend.
    - [ ] Add detailed setup instructions for the backend.
- [ ] **Refactor `hn2sn.py`**:
    - [ ] Ensure functions are well-defined and can be imported as a library by the FastAPI backend.
    - [ ] Separate core logic from script-specific execution (e.g., `if __name__ == "__main__":`).

This list provides a good starting point for the next steps.
Let me know what you'd like to tackle first! 