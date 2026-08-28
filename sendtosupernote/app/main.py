from fastapi import FastAPI, HTTPException, Depends, BackgroundTasks, Header
from fastapi.security import OAuth2PasswordBearer
from pydantic import BaseModel, HttpUrl, validator # Import validator
from typing import Optional, Dict
from threading import Lock, Semaphore
from datetime import datetime, timedelta, timezone
import uuid
import logging
import os
import tempfile # For temporary PDF file storage
from pathlib import Path # For path manipulation
import re # For path validation
from urllib.parse import urlparse # Added for domain extraction
import shutil # Added for temporary directory cleanup
import json

from . import processing # Import the processing module
from .sn_client import SNClientWithCSRF
from sncloud.exceptions import AuthenticationError as SNAuthenticationError

# --- Application Setup ---
app = FastAPI(
    title="SendToSupernote API",
    version="0.1.0",
    docs_url=None,
    redoc_url=None,
    openapi_url=None,
)

# Configure logging (basic setup)
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(name)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

# Runtime state contains task metadata only. Supernote session tokens remain
# in memory and are discarded whenever the service restarts.
STATE_DIR = Path(os.getenv("SENDTOSUPERNOTE_STATE_DIR", Path(__file__).parent))
TASK_STATUS_FILE_PATH = STATE_DIR / "task_status_store.json"
TASK_STATUS_TTL_DAYS = int(os.getenv("TASK_STATUS_TTL_DAYS", "30"))
TOKEN_TTL_HOURS = int(os.getenv("TOKEN_TTL_HOURS", "168"))
PROCESSING_CONCURRENCY = max(1, int(os.getenv("PROCESSING_CONCURRENCY", "2")))

# In-memory store for API tokens and their Supernote session tokens.
active_tokens: Dict[str, Dict[str, str]] = {}
task_statuses: Dict[str, Dict[str, str]] = {}
task_status_lock = Lock()
processing_slots = Semaphore(PROCESSING_CONCURRENCY)

def _utc_now() -> datetime:
    return datetime.now(timezone.utc)

def _utc_now_iso() -> str:
    return _utc_now().isoformat()

def _prune_task_statuses(statuses: Dict[str, Dict[str, str]]) -> Dict[str, Dict[str, str]]:
    cutoff = _utc_now() - timedelta(days=TASK_STATUS_TTL_DAYS)
    pruned: Dict[str, Dict[str, str]] = {}
    for task_id, record in statuses.items():
        updated_at = record.get("updated_at") or record.get("created_at")
        if not updated_at:
            pruned[task_id] = record
            continue
        try:
            updated_dt = datetime.fromisoformat(updated_at)
        except ValueError:
            pruned[task_id] = record
            continue
        if updated_dt.tzinfo is None:
            updated_dt = updated_dt.replace(tzinfo=timezone.utc)
        if updated_dt >= cutoff:
            pruned[task_id] = record
    return pruned

def load_task_statuses_from_file():
    global task_statuses
    if not TASK_STATUS_FILE_PATH.exists():
        task_statuses = {}
        return
    try:
        with TASK_STATUS_FILE_PATH.open("r") as status_file:
            data = json.load(status_file)
        task_statuses = _prune_task_statuses(data) if isinstance(data, dict) else {}
    except (json.JSONDecodeError, OSError) as exc:
        logger.error("Could not load task statuses: %s", exc)
        task_statuses = {}

def save_task_statuses_to_file():
    try:
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        pruned = _prune_task_statuses(task_statuses)
        with TASK_STATUS_FILE_PATH.open("w") as status_file:
            json.dump(pruned, status_file, indent=2)
    except OSError as exc:
        logger.error("Could not save task statuses: %s", exc)

def update_task_status(task_id: str, status: Optional[str] = None, message: Optional[str] = None, **fields):
    now = _utc_now_iso()
    with task_status_lock:
        record = task_statuses.get(task_id, {})
        record.setdefault("created_at", now)
        record["updated_at"] = now
        if status:
            record["status"] = status
        if message is not None:
            record["message"] = message
        for key, value in fields.items():
            if value is not None:
                record[key] = value
        task_statuses[task_id] = record
    save_task_statuses_to_file()
    return record

def get_task_status_record(task_id: str) -> Optional[Dict[str, str]]:
    with task_status_lock:
        record = task_statuses.get(task_id)
        return dict(record) if record else None

# --- Pydantic Models ---
class LoginRequest(BaseModel):
    supernote_email: str
    supernote_password: str

class TokenResponse(BaseModel):
    access_token: str
    token_type: str = "bearer"

class ArticleQueueRequest(BaseModel):
    url: HttpUrl
    html_content: Optional[str] = None
    target_path: Optional[str] = None       # e.g., /Inbox/MyArticles or from user config
    pdf_font_size: Optional[str] = None   # e.g., "12pt" or from user config
    source_identifier: Optional[str] = "WebApp" # To help name the PDF

    @validator('target_path')
    def validate_target_path(cls, v):
        if v is None: # Optional field, so None is fine
            return v
        if not v.startswith('/'):
            raise ValueError('target_path must start with "/".')
        # Basic check for invalid characters in a path segment. 
        # Supernote path segments likely don't allow these, but SNClient might handle/sanitize them.
        # This is a stricter check at the API level.
        if ':' in v or '*' in v or '?' in v or '"' in v or '<' in v or '>' in v or '|' in v:
            raise ValueError('target_path contains invalid characters (e.g., :*?"<>|).')
        if '//' in v: # Avoid double slashes that might be problematic
            raise ValueError('target_path cannot contain "//".')
        # Check for segments that are just dots (e.g. /./ or /../)
        path_segments = v.split('/')
        for segment in path_segments:
            if segment == '.' or segment == '..':
                 raise ValueError("target_path cannot contain '.' or '..' segments.")
        return v

    @validator('pdf_font_size')
    def validate_pdf_font_size(cls, v):
        if v is None: # Optional field
            return v
        # Regex to match common font size units like pt, px, em, rem, %
        # Allows integers or decimals
        if not re.match(r"^\\d*\\.?\\d+(pt|px|em|rem|%)$", v.lower()):
            raise ValueError('pdf_font_size must be a valid size (e.g., "12pt", "1.1em", "100%").')
        return v
    
    @validator('html_content')
    def validate_html_content_length(cls, v):
        if v is None:
            return v
        MAX_HTML_LENGTH = 10 * 1024 * 1024 # 10 MB limit for raw HTML
        if len(v) > MAX_HTML_LENGTH:
            raise ValueError(f'html_content is too large (>{MAX_HTML_LENGTH // (1024*1024)}MB).')
        return v

class ArticleQueueResponse(BaseModel):
    message: str
    task_id: str
    article_url: HttpUrl

class TaskStatusResponse(BaseModel):
    task_id: str
    status: str
    message: Optional[str] = None
    article_url: Optional[str] = None
    created_at: Optional[str] = None
    updated_at: Optional[str] = None
    pdf_filename: Optional[str] = None

class UserInfo(BaseModel):
    email: str
    supernote_token: str

# --- Authentication Dependencies ---
async def get_validated_user_info_from_token(authorization: Optional[str] = Header(None)) -> UserInfo:
    if authorization is None:
        logger.warning("Authorization header missing")
        raise HTTPException(
            status_code=401,
            detail="Not authenticated: Authorization header missing",
            headers={"WWW-Authenticate": "Bearer"},
        )
    
    parts = authorization.split()
    if len(parts) != 2 or parts[0].lower() != "bearer":
        logger.warning(f"Malformed Authorization header: {authorization}")
        raise HTTPException(
            status_code=401,
            detail="Not authenticated: Invalid token format. Expected 'Bearer <token>'",
            headers={"WWW-Authenticate": "Bearer"},
        )
    
    token = parts[1]
    user_creds = active_tokens.get(token)
    if not user_creds:
        logger.warning("Invalid or expired bearer token")
        raise HTTPException(
            status_code=401,
            detail="Not authenticated: Invalid or expired token",
            headers={"WWW-Authenticate": "Bearer"},
        )
    
    expires_at = datetime.fromisoformat(user_creds["expires_at"])
    if expires_at <= _utc_now():
        active_tokens.pop(token, None)
        raise HTTPException(
            status_code=401,
            detail="Not authenticated: Token expired",
            headers={"WWW-Authenticate": "Bearer"},
        )
    logger.info(f"Successfully authenticated user {user_creds['email']} with token.")
    return UserInfo(
        email=user_creds["email"],
        supernote_token=user_creds["supernote_token"],
    )

async def authenticate_with_supernote(email: str, password: str) -> Optional[str]:
    logger.info(f"Attempting Supernote login for {email}")
    if not email or not password:
        logger.warning("Supernote email or password not provided for verification.")
        return None
    try:
        client = SNClientWithCSRF()
        token = client.login(email, password)
        logger.info(f"Supernote login successful for {email}")
        return token
    except SNAuthenticationError as e:
        logger.error(f"Supernote login failed for {email}: {e}")
        return None
    except Exception as e:
        logger.error(f"Supernote Cloud login service error for {email}: {e}")
        raise

# --- API Endpoints ---
@app.on_event("startup")
async def startup_event():
    logger.info("Application startup: SendToSupernote API is starting.")
    
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    load_task_statuses_from_file()
    
    # Configure Gemini API Key if present
    gemini_api_key = os.getenv("GEMINI_API_KEY")
    if gemini_api_key:
        try:
            import google.generativeai as genai
            genai.configure(api_key=gemini_api_key)
            logger.info("Gemini API configured successfully at startup.")
        except ImportError:
            logger.error("Attempted to configure Gemini API, but google-generativeai library is not installed.")
        except Exception as e:
            logger.error(f"Error configuring Gemini API at startup: {e}")
    else:
        logger.warning("GEMINI_API_KEY not found in environment. AI features will be skipped if attempted.")

@app.get("/")
async def read_root():
    return {"message": "Welcome to the SendToSupernote API"}

@app.get("/healthz")
async def healthcheck():
    return {"status": "ok"}

@app.post("/api/auth/login", response_model=TokenResponse)
async def login_for_access_token(form_data: LoginRequest):
    try:
        supernote_token = await authenticate_with_supernote(
            form_data.supernote_email, form_data.supernote_password
        )
    except Exception as exc:
        raise HTTPException(
            status_code=502,
            detail="Supernote Cloud login is temporarily unavailable",
        ) from exc
    if not supernote_token:
        raise HTTPException(
            status_code=401,
            detail="Incorrect Supernote email or password",
            headers={"WWW-Authenticate": "Bearer"}, # Though not strictly bearer if we don't use OAuth2PasswordBearer fully
        )
    access_token = str(uuid.uuid4()) # Simple UUID token
    active_tokens[access_token] = {
        "email": form_data.supernote_email,
        "supernote_token": supernote_token,
        "expires_at": (_utc_now() + timedelta(hours=TOKEN_TTL_HOURS)).isoformat(),
    }
    logger.info("Generated an access token for %s", form_data.supernote_email)
    
    return TokenResponse(access_token=access_token)

def process_article_in_background(request_data: ArticleQueueRequest, user_info: UserInfo, task_id: str):
    logger.info(f"[Task {task_id}] Starting background processing for URL: {request_data.url} for user {user_info.email}")
    update_task_status(task_id, status="processing", message="Processing started")
    temp_dir_path_str = None # Initialize to None
    actual_pdf_path = None # Path to the PDF with the desired name
    
    processing_slots.acquire()
    try:
        # 1. Scrape content using the Playwright/Trafilatura pipeline from processing.py
        logger.info(f"[Task {task_id}] Scraping content for {request_data.url}")
        scraped_content = processing.scrape_article_content(
            url=str(request_data.url), 
            raw_html_from_extension=request_data.html_content
        )

        if not scraped_content or not scraped_content.get('plain_text'):
            logger.error(f"[Task {task_id}] Scraping failed or no plain text for {request_data.url}. Aborting.")
            update_task_status(task_id, status="failed", message="Scraping failed or no content found")
            return

        article_title = scraped_content.get('title', "Untitled Article")
        plain_text = scraped_content.get('plain_text')
        cleaned_html = scraped_content.get('html_content') 
        publish_date_str = scraped_content.get('extracted_date')
        author_name = scraped_content.get('author') 

        # Fallback for Author Name
        if not author_name or author_name.strip() == "":
            try:
                parsed_url = urlparse(str(request_data.url))
                author_name = parsed_url.netloc 
                logger.info(f"[Task {task_id}] Author not found, using domain as fallback: {author_name}")
            except Exception as e_urlparse:
                logger.warning(f"[Task {task_id}] Could not parse URL for author fallback: {e_urlparse}")
                author_name = "Unknown_Source"

        # 2. Classify article quality
        logger.info(f"Task {task_id}: Classifying article quality for: {article_title}")
        is_thought_provoking = processing.classify_article_quality(plain_text)
        if not is_thought_provoking:
            logger.info(f"Task {task_id}: Article '{article_title}' classified as advertisement/low-quality. Skipping PDF generation.")
            update_task_status(task_id, "skipped", f"Article '{article_title}' skipped: Classified as low-quality.")
            return
        logger.info(f"Task {task_id}: Article '{article_title}' classified as thought-provoking.")

        # 3. Reformat to Markdown using Gemini
        markdown_content = None
        extracted_publish_date = publish_date_str # Can be None
        image_urls_for_gemini = scraped_content.get('image_urls', []) # Get image_urls, default to empty list

        logger.info(f"Task {task_id}: Reformatting to Markdown for: {article_title}. Providing {len(image_urls_for_gemini)} image URLs to Gemini.")
        if scraped_content.get('plain_text'): # Ensure plain_text exists
            markdown_content = processing.reformat_to_markdown_gemini(
                article_text=scraped_content['plain_text'],
                article_url=str(request_data.url),
                article_publish_date_str=extracted_publish_date, # Pass the string date
                image_urls=image_urls_for_gemini # Pass image URLs
            )

        # 4. Generate PDF
        content_for_pdf_conversion = markdown_content
        html_to_render = ""
        pdf_font_size = request_data.pdf_font_size or "14pt" 

        if content_for_pdf_conversion:
            logger.info(f"[Task {task_id}] Converting Markdown to styled HTML for '{article_title}'.")
            html_to_render = processing.convert_markdown_to_styled_html(
                markdown_string=content_for_pdf_conversion, 
                font_size=pdf_font_size,
                document_title=article_title
            )
        elif cleaned_html:
            logger.info(f"[Task {task_id}] Using pre-cleaned HTML for '{article_title}' (Markdown failed or skipped).")
            # Get only the CSS string using the new parameter
            retrieved_markdown_css = processing.convert_markdown_to_styled_html(
                markdown_string="", # Dummy markdown, not used when getting CSS only
                font_size=pdf_font_size,
                return_css_only=True
            )
            html_to_render = f"""
            <!DOCTYPE html><html lang="en"><head><meta charset="UTF-8"><title>{article_title}</title>
            <style>{retrieved_markdown_css}</style></head><body>{cleaned_html}</body></html>
            """
            logger.info(f"[Task {task_id}] Wrapped cleaned HTML with styles for '{article_title}'.")
        else:
            logger.error(f"[Task {task_id}] No suitable content (Markdown or Cleaned HTML) to generate PDF for {article_title}. Aborting.")
            return

        if not html_to_render:
            logger.error(f"[Task {task_id}] HTML for PDF rendering is empty for '{article_title}'. Aborting.")
            return

        # 5. Generate PDF with desired filename in a temporary directory
        pdf_filename_for_supernote = processing.generate_supernote_pdf_filename(
            article_title=article_title, 
            author_name=author_name 
        )
        
        temp_dir_path_str = tempfile.mkdtemp(prefix=f"sn_pdf_{task_id}_")
        actual_pdf_path = Path(temp_dir_path_str) / pdf_filename_for_supernote
        
        logger.info(f"[Task {task_id}] Generating PDF as: {actual_pdf_path} for article '{article_title}'.")
        pdf_generated = processing.generate_pdf_from_html(html_content=html_to_render, output_pdf_path=str(actual_pdf_path))

        if not pdf_generated:
            logger.error(f"[Task {task_id}] PDF generation failed for '{article_title}'. Aborting.")
            update_task_status(task_id, status="failed", message="PDF generation failed")
            # No need to remove actual_pdf_path here, as the whole temp_dir will be removed in finally
            return
        logger.info(f"[Task {task_id}] PDF generated successfully: {actual_pdf_path} for '{article_title}'.")

        # 6. Upload PDF to Supernote
        logger.info(f"[Task {task_id}] Uploading PDF {actual_pdf_path.name} to Supernote for user {user_info.email}")
        uploaded_count = processing.upload_pdfs_to_supernote(
            pdf_filepaths=[str(actual_pdf_path)], # Pass the path to the PDF with the correct name
            sn_email=user_info.email,
            sn_access_token=user_info.supernote_token,
            sn_target_path=request_data.target_path 
        )

        if uploaded_count > 0:
            logger.info(f"[Task {task_id}] Successfully uploaded {actual_pdf_path.name} to Supernote for '{article_title}'.")
            update_task_status(task_id, status="uploaded", message="Uploaded to Supernote", pdf_filename=actual_pdf_path.name)
        else:
            logger.error(f"[Task {task_id}] Failed to upload {actual_pdf_path.name} to Supernote for '{article_title}'.")
            update_task_status(task_id, status="failed", message="Upload failed")

    except Exception as e:
        logger.error(f"[Task {task_id}] Unhandled error in background processing for {request_data.url}: {e}")
        update_task_status(task_id, status="failed", message=str(e))
        import traceback
        logger.error(traceback.format_exc())
    finally:
        if temp_dir_path_str and os.path.exists(temp_dir_path_str):
            try:
                shutil.rmtree(temp_dir_path_str) # Use shutil.rmtree for directory
                logger.info(f"[Task {task_id}] Cleaned up temporary directory: {temp_dir_path_str}")
            except Exception as e_clean:
                logger.error(f"[Task {task_id}] Error cleaning up temp directory {temp_dir_path_str}: {e_clean}")
        processing_slots.release()
        
    logger.info(f"[Task {task_id}] Finished background processing for {request_data.url}")

@app.post("/api/queue_article", response_model=ArticleQueueResponse)
async def queue_article(request_data: ArticleQueueRequest, background_tasks: BackgroundTasks, current_user: UserInfo = Depends(get_validated_user_info_from_token)):
    task_id = str(uuid.uuid4())
    logger.info(f"Queueing article: {request_data.url} for user {current_user.email} with Task ID: {task_id}")
    update_task_status(
        task_id,
        status="queued",
        message="Article queued for processing",
        url=str(request_data.url),
        email=current_user.email,
        target_path=request_data.target_path,
    )
    background_tasks.add_task(process_article_in_background, request_data, current_user, task_id)
    return ArticleQueueResponse(
        message="Article successfully queued for processing.", 
        task_id=task_id,
        article_url=request_data.url
    )

@app.get("/api/task/{task_id}", response_model=TaskStatusResponse)
async def get_task_status(task_id: str, current_user: UserInfo = Depends(get_validated_user_info_from_token)):
    record = get_task_status_record(task_id)
    if not record or record.get("email") != current_user.email:
        raise HTTPException(status_code=404, detail="Task not found")
    return TaskStatusResponse(
        task_id=task_id,
        status=record.get("status", "unknown"),
        message=record.get("message"),
        article_url=record.get("url"),
        created_at=record.get("created_at"),
        updated_at=record.get("updated_at"),
        pdf_filename=record.get("pdf_filename"),
    )

if __name__ == "__main__":
    import uvicorn
    # When running with "python -m sendtosupernote.app.main", uvicorn needs the full path.
    uvicorn.run("sendtosupernote.app.main:app", host="127.0.0.1", port=8765)
