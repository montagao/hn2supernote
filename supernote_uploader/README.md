# Supernote Uploader

A Python library and CLI for uploading files to Supernote Cloud.

## Installation

```bash
pip install supernote-uploader
```

## CLI Usage

### Login (caches credentials)

```bash
supernote login --email user@example.com --password yourpassword
```

### Upload files

```bash
# Upload a single file to /Inbox (default)
supernote upload article.pdf

# Upload multiple files to a specific folder
supernote upload *.pdf --folder /Documents/Articles

# Upload with credentials (or use SUPERNOTE_EMAIL/SUPERNOTE_PASSWORD env vars)
supernote upload book.epub -e user@example.com -p password
```

### List folder contents

```bash
supernote ls
supernote ls /Inbox
supernote ls /Documents/Articles
```

### Create folders

```bash
supernote mkdir /Inbox/Articles
supernote mkdir /Documents/Work/Projects --parents
```

## Python API Usage

```python
from supernote_uploader import SupernoteClient

# Using context manager (recommended)
with SupernoteClient("user@example.com", "password") as client:
    result = client.upload("article.pdf", "/Inbox/Articles")
    print(f"Upload {'succeeded' if result.success else 'failed'}")

# Manual session management
client = SupernoteClient()
client.login("user@example.com", "password")
client.upload("book.epub", "/Documents")
client.close()

# Batch upload
results = client.upload_many(["a.pdf", "b.pdf"], "/Inbox")

# Folder operations
client.mkdir("/Inbox/Articles", parents=True)
items = client.list_folder("/Inbox")
```

## Email Verification

If your account requires email verification:

### CLI

The CLI will prompt for the verification code automatically.

### Python API

```python
from supernote_uploader import SupernoteClient, VerificationRequiredError

try:
    client = SupernoteClient("user@example.com", "password")
except VerificationRequiredError as e:
    # A verification code was sent to your email
    code = input("Enter verification code: ")
    client = SupernoteClient(auto_login=False)
    client.verify(code, e.verification_context)
    # Now authenticated
```

## Environment Variables

- `SUPERNOTE_EMAIL` - Default email for authentication
- `SUPERNOTE_PASSWORD` - Default password for authentication

## API Reference

### SupernoteClient

- `login(email, password)` - Login to Supernote Cloud
- `verify(code, context)` - Complete login with email verification code
- `upload(file_path, target_folder, create_folder=True)` - Upload a file
- `upload_many(file_paths, target_folder, create_folder=True, stop_on_error=False)` - Upload multiple files
- `list_folder(folder_path)` - List folder contents
- `mkdir(folder_path, parents=False)` - Create a folder
- `folder_exists(folder_path)` - Check if folder exists
- `close()` - Close the client

## License

MIT
