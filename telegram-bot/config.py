"""
Configuration management for the Telegram bot.
Loads environment variables and validates required settings.
"""

import os
from dotenv import load_dotenv

# Load environment variables from .env file
load_dotenv()


def get_config():
    """
    Load and validate configuration from environment variables.
    Returns a dict with all configuration values.
    Raises ValueError if required variables are missing.
    """
    config = {
        # Required
        "TELEGRAM_BOT_TOKEN": os.getenv("TELEGRAM_BOT_TOKEN"),
        "SN_EMAIL": os.getenv("SN_EMAIL"),
        "SN_PASSWORD": os.getenv("SN_PASSWORD"),
        "GEMINI_API_KEY": os.getenv("GEMINI_API_KEY"),
        # Optional with defaults
        "SUPERNOTE_TARGET_PATH": os.getenv("SUPERNOTE_TARGET_PATH", "/Inbox/SendToSupernote"),
        "PDF_FONT_SIZE": os.getenv("PDF_FONT_SIZE", "14pt"),
    }

    # Validate required variables
    missing = []
    for key in ["TELEGRAM_BOT_TOKEN", "SN_EMAIL", "SN_PASSWORD", "GEMINI_API_KEY"]:
        if not config[key]:
            missing.append(key)

    if missing:
        raise ValueError(f"Missing required environment variables: {', '.join(missing)}")

    return config


def validate_config():
    """
    Validate that all required configuration is present.
    Call this at startup to fail fast if config is incomplete.
    """
    try:
        get_config()
        return True
    except ValueError as e:
        print(f"Configuration error: {e}")
        return False
