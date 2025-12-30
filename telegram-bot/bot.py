#!/usr/bin/env python3
"""
Telegram bot that processes URLs and sends them to Supernote as PDFs.

Usage:
    1. Send any URL to the bot
    2. Bot will scrape, convert to PDF, and upload to your Supernote device
"""

import asyncio
import logging
import re
from telegram import Update
from telegram.ext import Application, CommandHandler, MessageHandler, filters, ContextTypes

from config import get_config, validate_config
from processing import process_url

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
        "/help - Show this help message"
    )
    await update.message.reply_text(help_text)


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
        success, message = await asyncio.to_thread(
            process_url,
            url=url,
            gemini_api_key=config["GEMINI_API_KEY"],
            sn_email=config["SN_EMAIL"],
            sn_password=config["SN_PASSWORD"],
            sn_target_path=config["SUPERNOTE_TARGET_PATH"],
            font_size=config["PDF_FONT_SIZE"],
            skip_quality_check=False
        )

        if success:
            await status_message.edit_text(f"Done! {message}")
        else:
            await status_message.edit_text(f"Failed: {message}")

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
    application.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, handle_message))

    # Start the bot
    logger.info("Starting bot...")
    application.run_polling(allowed_updates=Update.ALL_TYPES)


if __name__ == "__main__":
    main()
