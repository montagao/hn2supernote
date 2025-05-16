### 1. Goal

Ship a minimal Chrome extension plus a small Python API so I can click one button in Chrome and queue the current page for delivery to my Supernote A5X2.
All AI, PDF, and Supernote work stays on the server; the extension only hands over data.

---

### 2. High-level flow

1. User installs the extension and opens the options page.
2. User enters:

   * Backend URL (our FastAPI service)
   * Supernote email
   * Supernote password
     Credentials are stored with `chrome.storage.sync`.
3. While browsing, user clicks the toolbar icon.
4. Extension grabs the tab URL and, via content script, optional cleaned HTML (`Readability.js`).
5. Extension `POST`s JSON to `POST <backend>/api/queue_article`.
6. Backend runs the existing pipeline: scrape → Gemini Markdown → styled HTML → PDF → Supernote.
7. Extension shows a toast: success or error.
8. Options page and popup both display a warning: *“Traffic passes through US servers only. EU route in future.”*

---

### 3. Repo layout

```
send-to-supernote/
├── backend/
│   ├── app.py              # FastAPI entry
│   ├── pipeline/           # split functions from your big script
│   │   ├── core.py         # scrape, markdown, pdf, upload
│   │   └── task.py         # process_url()
│   ├── requirements.in
│   └── requirements.txt    # generated with pip-compile
└── extension/
    ├── manifest.json
    ├── popup.html
    ├── popup.js
    ├── options.html
    ├── options.js
    ├── content.js          # Readability scrape (optional)
    └── icon128.png
```

Use `venv` + `pip-tools` in `backend`, `npm` only for any tooling in `extension` (not required at runtime).

---

### 4. Backend

#### 4.1 `app.py`

```python
from fastapi import FastAPI, HTTPException
from pydantic import BaseModel
from pipeline.task import process_url

app = FastAPI()

class Req(BaseModel):
    url: str
    html: str | None = None
    email: str
    password: str

@app.post("/api/queue_article")
async def queue_article(req: Req):
    try:
        pdf_name = process_url(req.url, req.html, req.email, req.password)
        return {"status": "queued", "file": pdf_name}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))
```

*Within `process_url` pass `email` and `password` to `upload_to_supernote`, overriding env vars if present.*

#### 4.2 Run

```bash
python -m venv venv && source venv/bin/activate
pip install -r requirements.txt
uvicorn app:app --host 0.0.0.0 --port 8123 --reload
```

Expose with Caddy or nginx if TLS is needed.

---

### 5. Chrome extension

#### 5.1 `manifest.json` (MV3)

```json
{
  "name": "Send to Supernote",
  "description": "Queue current page for Supernote",
  "version": "1.0.0",
  "manifest_version": 3,
  "permissions": [
    "activeTab",
    "scripting",
    "storage",
    "notifications"
  ],
  "host_permissions": [
    "http://localhost:8123/*",
    "https://sn-backend.example.com/*"
  ],
  "action": {
    "default_popup": "popup.html",
    "default_title": "Send to Supernote"
  },
  "options_page": "options.html",
  "icons": { "128": "icon128.png" }
}
```

#### 5.2 `options.html`

```html
<!doctype html>
<html>
<head><meta charset="utf-8"><title>Supernote setup</title></head>
<body style="font-family:sans-serif;max-width:420px;margin:2em">
  <h2>Supernote connection</h2>

  <label>Backend URL<br>
    <input id="backend" type="url" style="width:100%" placeholder="https://sn-backend.example.com">
  </label><br><br>

  <label>Email<br>
    <input id="email" type="email" style="width:100%">
  </label><br><br>

  <label>Password<br>
    <input id="password" type="password" style="width:100%">
  </label><br><br>

  <button id="save">Save</button>

  <p style="margin-top:1.5em;color:#c00">
    Note: traffic routes through US servers only.
  </p>

  <script src="options.js"></script>
</body>
</html>
```

#### 5.3 `options.js`

```js
const ids = ['backend', 'email', 'password'];

function load() {
  chrome.storage.sync.get(ids, data => {
    ids.forEach(id => { if (data[id]) document.getElementById(id).value = data[id]; });
  });
}

function save() {
  const obj = {};
  ids.forEach(id => obj[id] = document.getElementById(id).value.trim());
  chrome.storage.sync.set(obj, () => alert('Saved ✓'));
}

document.getElementById('save').addEventListener('click', save);
load();
```

#### 5.4 `popup.html`

```html
<!doctype html>
<html>
<head><meta charset="utf-8"><title>Send</title></head>
<body style="min-width:220px;font-family:sans-serif">
  <button id="sendBtn" title="Uses US servers only">Send current page</button>
  <p id="status"></p>
  <script src="popup.js"></script>
</body>
</html>
```

#### 5.5 `popup.js`

```js
async function cfg() {
  return new Promise(r => chrome.storage.sync.get(['backend','email','password'], r));
}

document.getElementById('sendBtn').addEventListener('click', async () => {
  const {backend,email,password} = await cfg();
  if (!backend || !email || !password) {
    return window.open(chrome.runtime.getURL('options.html'));
  }

  const [tab] = await chrome.tabs.query({active:true,currentWindow:true});
  const html = await chrome.tabs.sendMessage(tab.id, {action:'extract'});

  const res = await fetch(`${backend}/api/queue_article`, {
    method:'POST',
    headers:{'Content-Type':'application/json'},
    body:JSON.stringify({url:tab.url, html, email, password})
  });

  const msg = document.getElementById('status');
  msg.textContent = res.ok ? 'Queued ✓' : `Error: ${await res.text()}`;
});
```

#### 5.6 `content.js` (optional readability)

```js
importScripts('https://unpkg.com/@mozilla/readability/dist/Readability.js');

chrome.runtime.onMessage.addListener((m, s, send) => {
  if (m.action === 'extract') {
    try {
      const doc = new DOMParser().parseFromString(document.documentElement.outerHTML, 'text/html');
      send(new Readability(doc).parse()?.content || null);
    } catch { send(null); }
  }
  return true;
});
```

Add to manifest:

```json
"content_scripts": [
  {
    "matches": ["<all_urls>"],
    "js": ["content.js"],
    "run_at": "document_idle"
  }
]
```

---

### 6. Build and run

```bash
# backend
cd backend
python -m venv venv && . venv/bin/activate
pip install -r requirements.txt
uvicorn app:app --host 0.0.0.0 --port 8123 --reload

# extension
# go to chrome://extensions → Load unpacked → select extension/ folder
```

---

### 7. Testing checklist

* Fresh install → click icon → opens setup page.
* Save creds → click icon → status “Queued ✓” with running backend.
* Stop backend → click icon → shows error.
* Page with paywall → still queues URL, backend handles scrape.
* Very short page → backend rejects as “too short” message in logs.
* Multiple clicks on same page → backend deduplicates by history file.

---

### 8. Stretch goals

* Dark vs light CSS toggle in options.
* Job history list in popup via `/api/jobs`.
* Progress notifications with `chrome.notifications`.
* EU region backend choice.

---

### 9. Deliverables

1. Git repo with logical commits (edit in **vim**, no nano).
2. `README.md` – setup instructions, `.env.example`.
3. Short GIF or Loom demo showing end-to-end flow.
4. List of known issues and todos.

No em-dashes used. Ask if anything is unclear.
