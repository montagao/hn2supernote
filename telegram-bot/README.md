# Telegram Bot for SendToSupernote

A Telegram bot that converts web articles into PDFs and uploads them to your Supernote device.

## Features

- Send any URL to the bot
- Automatically extracts article content using Playwright + Trafilatura
- Uses Gemini AI to clean and reformat content
- Generates a styled PDF
- Uploads directly to your Supernote cloud storage

## Setup

### 1. Create a Telegram Bot

1. Open Telegram and search for [@BotFather](https://t.me/botfather)
2. Send `/newbot` command
3. Follow the prompts:
   - Choose a name for your bot (e.g., "My Supernote Bot")
   - Choose a username for your bot (must end in `bot`, e.g., "my_supernote_bot")
4. BotFather will give you an HTTP API token - save this for later
5. (Optional) Set bot commands:
   ```
   /setcommands
   ```
   Then select your bot and send:
   ```
   start - Show welcome message
   help - Show usage instructions
   ```

### 2. Get API Keys

- **Gemini API Key**: Get from [Google AI Studio](https://aistudio.google.com/apikey)
- **Supernote Credentials**: Your Supernote cloud account email and password

### 3. Install Dependencies

```bash
cd telegram-bot

# Install dependencies with uv
uv sync

# Install Playwright browsers
uv run playwright install chromium firefox
```

### 4. Configure Environment

```bash
cp .env.example .env
```

Edit `.env` with your credentials:

```
TELEGRAM_BOT_TOKEN=your_bot_token_from_botfather
SN_EMAIL=your_supernote_email
SN_PASSWORD=your_supernote_password
GEMINI_API_KEY=your_gemini_api_key
```

Optional settings:
```
SUPERNOTE_TARGET_PATH=/Inbox/SendToSupernote
PDF_FONT_SIZE=14pt
```

### 5. Run the Bot

```bash
uv run python bot.py
```

The bot will start polling for messages. Keep it running to receive URLs.

## Usage

1. Open your bot in Telegram (search for the username you created)
2. Send `/start` to see the welcome message
3. Paste any URL (article, blog post, Twitter/X link, etc.)
4. Wait for processing (usually 30-60 seconds)
5. The PDF will appear on your Supernote device

## Supported Content

- Blog posts and articles
- Twitter/X posts
- News articles
- Any webpage with readable content

## Troubleshooting

### "Configuration error: Missing required environment variables"
Make sure your `.env` file exists and contains all required variables.

### "Failed to scrape article content"
The webpage might be blocking automated access or have no extractable content.

### "Failed to upload to Supernote"
Check your Supernote credentials and ensure the target folder path is valid.

### Bot not responding
- Make sure the bot is running (`uv run python bot.py`)
- Check the console for error messages
- Verify your Telegram bot token is correct

## File Structure

```
telegram-bot/
├── bot.py              # Telegram bot handlers
├── processing.py       # Content extraction & PDF pipeline
├── config.py           # Environment configuration
├── pyproject.toml      # Project config & dependencies (for uv)
├── .env.example        # Environment template
└── README.md           # This file
```
