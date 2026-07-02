# TranslateMom — business and app context (distilled from the internal dossier, July 2026)

## Product
TranslateMom (https://www.translate.mom) is a web-based AI video translation studio for
transcription, subtitles, captions, and dubbing. Users upload audio/video files or import links
(YouTube, TikTok, X, Instagram), generate transcripts and translated subtitles, edit timing and
style in a browser-based Studio, then export SRT/VTT/ASS/TXT or burned-in MP4. Operator:
15509947 Canada Inc.; solo founder/operator voice is Monta (@montakaoh). Support: hello@translate.mom.

## Customers and jobs
Video creators, translators, accessibility professionals, course producers, marketers, and small
teams. Higher-value target segments per strategy docs: pro subtitlers (browser-native ASS depth),
agencies with recurring localization work, and accessibility-compliance buyers. Core jobs: import
media or link → timed transcript → translation → optional AI dubbing → edit text/timing/style →
export files or burned-in video → share public watch pages and collect suggestions.

## Business model and metrics
Credit-based subscriptions: Starter $9/mo 200 credits, Plus $18/mo 600, Pro $44/mo 2000, plus a
Lifetime plan (2000 credits/mo) and permanent credit packs (iOS/RevenueCat). Credit costs:
transcription+translation 1 credit/min, Gemini pro transcription 2/min, AI dubbing 35/min. Editing,
styling, exports, and subtitle imports consume no credits. Payments: Stripe (checkout, portal,
webhooks, trials), RevenueCat for iOS IAP, legacy CoinPayments. Snapshot 2026-07-01: ~$4,893 MRR /
$58,716 ARR, 299 paid subscriptions, ~20-23% monthly churn proxy. **Retention/churn is the #1
business risk** — internal strategy says fix the bucket before the faucet: dunning/revenue
recovery, cancellation saves and exit surveys, transparent cost gates (especially dubbing), annual
plan nudges, credit packs — before adding raw AI features.

## Differentiators (per internal strategy)
Browser-native ASS styling fidelity (JASSUB/libass — rare among SaaS competitors), bilingual/
dual-language subtitle layouts, speaker-aware presentation, deep correction workflow after AI
output, owned exports (vs platform auto-captions), custom dictionaries/terminology, honest
creator pricing. Known weakness: the landing/onboarding hides the "magic" behind a queue — no
fast visible transformation before signup.

## Architecture (monorepo with app submodules)
- **apps/tmomskit** — SvelteKit (Svelte 4) web app: marketing/SEO/blog/tool pages, pricing,
  login/signup, /app/newtask, /app/task/[taskId], /watch/[taskId], the Studio editor shell,
  Firebase client auth, task history, sharing, legal. Stack: Vite, Tailwind, Firebase, JASSUB/
  libass, ass-compiler, wavesurfer, mediabunny, Sentry, Vitest.
- **apps/transcribemom** — Node.js ESM Express backend (HTTPS :443) + separate worker process
  (src/server/worker.js). Presigned R2 uploads, task validation, Bull/Redis queues, Firestore,
  Cloudflare R2 artifacts, ffmpeg, provider adapters (OpenAI, Gemini, DeepL, Google Translate,
  ElevenLabs, Mistral, Replicate, Whisper, Scribe), bot automation endpoints behind X-API-Key,
  Bull Board admin at /admin. Key routes: /api/new-task/v2/*, /process-video-plus-r2 (paid),
  /process-video-r2 (free), /embed-subtitles-v3, /process-dubbing, /get-presigned-*,
  /download-youtube-video, public SEO tool endpoints. Queues include video/whisper/scribe/mistral
  transcription, translation, dubbing, embed-subtitle, audio-processing, alignment (NFA/Qwen),
  srt-refine, transcode, trim/rotate.
- **apps/webhooksmom** — Firebase Functions for billing/account state: Stripe customers/checkout/
  portal/webhooks, RevenueCat webhooks, trials, referral codes/awards, mail utilities. Contains
  **r2-access-worker**: Cloudflare Worker (KV + Durable Object) serving R2 media on
  usr2.translate.mom/* (bucket translatemomenam), tracking .mp4 access and cautious retention cleanup.
- **packages/karaoke-renderer** — published npm package (@montagao/karaoke-renderer) for
  canvas-based animated caption rendering.
- Adjacent assets: tmom-mail (campaigns), tmom-postiz (social scheduling).

## Data model
Canonical in-app subtitle truth is `Sub[]` in the subtitles store plus `assSettings` for visuals;
SRT/VTT/ASS are derived artifacts. Persisted text is SRT in R2 with Firestore pointers:
tasks/{taskId}/transcripts/current + history (last 10), saves coalesced ~1/s with SHA-256
change-skip; visual settings JSON persisted separately with content addressing + Firestore CAS.
Firestore: users/{uid} (plan/tier, subscription status, monthly/permanent credits, Stripe id,
referrals), tasks/{taskId} (status, srtFiles by language, artifacts, parentId), suggestions,
referral collections, newTaskV2StagedUploadClaims.

## Auth boundaries
Firebase Auth in browser (Google/magic-link); authenticated fetch attaches Firebase ID tokens;
transcribemom protects paid/new-task routes with firebaseAuthMiddleware; bot endpoints use API
keys; billing uses Firebase callable context + Stripe/RevenueCat webhook verification.

## Working rules and commands
Always run commands from the owning app, never the (stale) root README.
- tmomskit: `npm run dev` / `build` / `test` (Vitest) / `check` (svelte-check) / `lint` / `format`.
- transcribemom: `npm test` (Jest, experimental-vm-modules) / `npm run lint` / `start` / `start:worker`.
- webhooksmom: from functions/ — `npm run lint` / `serve` (emulators) / `deploy`. R2 worker has its
  own test/audit/deploy scripts.
App-level AGENTS.md outranks the root README. Some Jest suites warn/fail even when skipped.

## Known risks and cautions
- Claim drift in public copy: file-size limits (5GB vs 1GB claims), "99% accurate", "all
  languages", lifetime storage promises — verify before touching user-facing copy.
- Account deletion cleanup incomplete (Auth deleted, Firestore/R2/billing may remain).
- Service-account credentials flagged as needing to move out of firebase.json.
- Legacy subtitle paths still behind feature flags; billing has legacy price/product paths.
- R2 retention cleanup is operationally sensitive (dry-run defaults, active-read leases).
- Platform absorption risk (YouTube/CapCut auto-dubbing); dubbing margins sensitive to AI costs.
