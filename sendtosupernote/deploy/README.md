# VPS deployment

The production API runs as the `sendtosupernote` system user. Uvicorn listens
only on `127.0.0.1:8765`; nginx exposes `https://supernote.translate.mom`.

Runtime state is stored in `/var/lib/sendtosupernote`. Supernote credentials
are kept in process memory only and are discarded on service restart. Access
tokens expire after seven days by default.

Optional settings belong in `/etc/sendtosupernote.env`:

```dotenv
GEMINI_API_KEY=
SUPERNOTE_TARGET_PATH=/Inbox/SendToSupernote
PROCESSING_CONCURRENCY=2
TOKEN_TTL_HOURS=168
```

The Gemini key is optional. Without it, the backend uses the cleaned HTML sent
by the extension when it creates the PDF.

Install the pinned runtime dependencies from `requirements.txt`, then install
the matching browser bundle with `playwright install --with-deps chromium`.

After the DNS record points at the VPS, enable HTTPS with:

```bash
certbot --nginx -d supernote.translate.mom --redirect
```

The nginx site exposes only the root health information, `/healthz`, the login
endpoint, the article queue endpoint, and UUID-shaped task-status endpoints.
