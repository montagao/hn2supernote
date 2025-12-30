#!/usr/bin/env python3
"""
Telegram bot that processes URLs and sends them to Supernote as PDFs.

Usage:
    1. Send any URL to the bot
    2. Bot will scrape, convert to PDF, and upload to your Supernote device
"""

import asyncio
import json
import logging
import re
from datetime import datetime, timezone
from pathlib import Path
from telegram import Update
from telegram.ext import Application, CommandHandler, MessageHandler, filters, ContextTypes

from config import get_config, validate_config
from processing import process_url, verify_supernote_code

# History storage
HISTORY_PATH = Path(__file__).parent / "history.json"
MAX_HISTORY_ITEMS = 50


def _load_history() -> list[dict]:
    """Load history from JSON file."""
    if not HISTORY_PATH.exists():
        return []
    try:
        data = json.loads(HISTORY_PATH.read_text())
        return data if isinstance(data, list) else []
    except Exception:
        return []


def _save_history(history: list[dict]) -> None:
    """Save history to JSON file."""
    try:
        HISTORY_PATH.write_text(json.dumps(history, indent=2))
    except Exception as e:
        logging.getLogger(__name__).warning(f"Failed to save history: {e}")


def add_to_history(result: dict) -> None:
    """Add a processed article to history."""
    history = _load_history()
    entry = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "url": result.get("source_url"),
        "title": result.get("title"),
        "author": result.get("author"),
        "filename": result.get("filename"),
        "target_path": result.get("target_path"),
        "success": result.get("success", False),
    }
    history.insert(0, entry)
    # Keep only the most recent items
    history = history[:MAX_HISTORY_ITEMS]
    _save_history(history)

# Configure logging
logging.basicConfig(
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s',
    level=logging.INFO
)
logger = logging.getLogger(__name__)

# URL regex pattern
URL_PATTERN = re.compile(
    r'https?://[^\s<>"{}|\\^`\[\]]+'
)

# Pending verification info per Telegram user
pending_verifications: dict[int, dict] = {}


async def start(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle the /start command."""
    welcome_message = (
        "Welcome to SendToSupernote Bot!\n\n"
        "Send me any URL and I'll:\n"
        "1. Extract the article content\n"
        "2. Convert it to a clean PDF\n"
        "3. Upload it to your Supernote device\n\n"
        "Just paste a link to get started!"
    )
    await update.message.reply_text(welcome_message)


async def help_command(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle the /help command."""
    help_text = (
        "How to use this bot:\n\n"
        "1. Simply send me a URL (e.g., https://example.com/article)\n"
        "2. I'll process the page and send a PDF to your Supernote\n\n"
        "Supported links:\n"
        "- Blog posts and articles\n"
        "- Twitter/X posts\n"
        "- News articles\n"
        "- Any webpage with readable content\n\n"
        "Commands:\n"
        "/start - Show welcome message\n"
        "/help - Show this help message\n"
        "/history - Show recently sent articles\n"
        "/verify <code> - Verify Supernote login when prompted"
    )
    await update.message.reply_text(help_text)


async def history_command(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle the /history command."""
    history = _load_history()

    if not history:
        await update.message.reply_text("No articles sent yet.")
        return

    # Show last 10 items
    lines = ["Recent articles sent to Supernote:\n"]
    for i, entry in enumerate(history[:10], 1):
        # Parse timestamp
        try:
            dt = datetime.fromisoformat(entry["timestamp"].replace("Z", "+00:00"))
            date_str = dt.strftime("%b %d, %H:%M")
        except Exception:
            date_str = "Unknown"

        title = entry.get("title") or "Untitled"
        # Truncate long titles
        if len(title) > 40:
            title = title[:37] + "..."

        status = "OK" if entry.get("success") else "FAIL"
        lines.append(f"{i}. [{status}] {title}")
        lines.append(f"   {date_str}")

    await update.message.reply_text("\n".join(lines))


async def verify_command(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle verification code submissions."""
    user_id = update.effective_user.id
    if user_id not in pending_verifications:
        await update.message.reply_text(
            "No pending verification. Send a URL first, then use /verify if prompted."
        )
        return

    if not context.args:
        await update.message.reply_text("Usage: /verify <code>")
        return

    code = context.args[0].strip()
    info = pending_verifications[user_id]
    config = get_config()

    success, message = await asyncio.to_thread(
        verify_supernote_code,
        sn_email=info.get("email") or config["SN_EMAIL"],
        verification_code=code,
        valid_code_key=info["valid_code_key"],
        timestamp=info["timestamp"],
    )

    if success:
        pending_verifications.pop(user_id, None)
    await update.message.reply_text(message)


async def handle_message(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle incoming messages and process URLs."""
    message_text = update.message.text

    # Extract URLs from message
    urls = URL_PATTERN.findall(message_text)

    if not urls:
        await update.message.reply_text(
            "I didn't find any URLs in your message. Please send me a link to process."
        )
        return

    # Process the first URL found
    url = urls[0]
    logger.info(f"Processing URL from user {update.effective_user.id}: {url}")

    # Send processing status
    status_message = await update.message.reply_text(
        f"Processing: {url}\n\nThis may take a minute..."
    )

    try:
        # Get config
        config = get_config()

        # Run the processing pipeline in a thread to avoid blocking
        result = await asyncio.to_thread(
            process_url,
            url=url,
            gemini_api_key=config["GEMINI_API_KEY"],
            sn_email=config["SN_EMAIL"],
            sn_password=config["SN_PASSWORD"],
            sn_target_path=config["SUPERNOTE_TARGET_PATH"],
            font_size=config["PDF_FONT_SIZE"],
            skip_quality_check=False
        )

        if result.get("verification"):
            pending_verifications[update.effective_user.id] = result["verification"]
            await status_message.edit_text(result["message"])
            return

        # Save to history
        add_to_history(result)

        if result["success"]:
            # Format a nice success message
            lines = ["Sent to Supernote!"]
            if result.get("title"):
                lines.append(f"Title: {result['title']}")
            if result.get("author"):
                lines.append(f"Author: {result['author']}")
            if result.get("filename"):
                lines.append(f"File: {result['filename']}")
            if result.get("target_path"):
                lines.append(f"Location: {result['target_path']}")

            await status_message.edit_text("\n".join(lines))
        else:
            await status_message.edit_text(f"Failed: {result['message']}")

    except Exception as e:
        logger.error(f"Error processing URL {url}: {e}")
        await status_message.edit_text(
            f"Error processing URL: {str(e)}\n\nPlease try again later."
        )


def main() -> None:
    """Start the bot."""
    # Validate configuration
    if not validate_config():
        logger.error("Configuration validation failed. Please check your .env file.")
        return

    config = get_config()

    # Create the Application
    application = Application.builder().token(config["TELEGRAM_BOT_TOKEN"]).build()

    # Add handlers
    application.add_handler(CommandHandler("start", start))
    application.add_handler(CommandHandler("help", help_command))
    application.add_handler(CommandHandler("history", history_command))
    application.add_handler(CommandHandler("verify", verify_command))
    application.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, handle_message))

    # Start the bot
    logger.info("Starting bot...")
    application.run_polling(allowed_updates=Update.ALL_TYPES)


if __name__ == "__main__":
    main()
