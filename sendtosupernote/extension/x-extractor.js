(function attachXPostExtractor(root, factory) {
  const extractor = factory();

  if (typeof module === 'object' && module.exports) {
    module.exports = extractor;
  } else {
    root.XPostExtractor = extractor;
  }
})(typeof globalThis !== 'undefined' ? globalThis : this, function createXPostExtractor() {
  const X_HOSTS = new Set(['x.com', 'www.x.com', 'twitter.com', 'www.twitter.com']);
  const MAX_TITLE_EXCERPT_LENGTH = 72;

  function escapeHtml(value) {
    return String(value ?? '')
      .replaceAll('&', '&amp;')
      .replaceAll('<', '&lt;')
      .replaceAll('>', '&gt;')
      .replaceAll('"', '&quot;')
      .replaceAll("'", '&#39;');
  }

  function safeHttpUrl(value, baseUrl = 'https://x.com/') {
    if (!value) return null;

    try {
      const parsed = new URL(value, baseUrl);
      if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return null;
      return parsed.href;
    } catch (_error) {
      return null;
    }
  }

  function parseXStatusUrl(value) {
    const normalizedUrl = safeHttpUrl(value);
    if (!normalizedUrl) return null;

    const parsed = new URL(normalizedUrl);
    if (!X_HOSTS.has(parsed.hostname.toLowerCase())) return null;

    const match = parsed.pathname.match(/^\/([^/]+)\/status\/(\d+)(?:\/|$)/i);
    if (!match) return null;

    return {
      username: decodeURIComponent(match[1]),
      statusId: match[2],
      canonicalUrl: `https://x.com/${encodeURIComponent(match[1])}/status/${match[2]}`,
    };
  }

  function normalizedText(element) {
    if (!element) return '';
    return String(element.innerText || element.textContent || '')
      .replace(/\u00a0/g, ' ')
      .replace(/[ \t]+\n/g, '\n')
      .replace(/\n[ \t]+/g, '\n')
      .replace(/[ \t]{2,}/g, ' ')
      .trim();
  }

  function stripVerifiedLabel(value) {
    return value
      .replace(/\s*Verified account\s*/gi, ' ')
      .replace(/\s+/g, ' ')
      .trim();
  }

  function isTargetStatusLink(anchor, statusId) {
    const parsed = parseXStatusUrl(anchor?.getAttribute?.('href') || anchor?.href);
    return Boolean(parsed && parsed.statusId === statusId);
  }

  function findTargetArticle(doc, statusId) {
    for (const article of doc.querySelectorAll('article')) {
      const matchingTimeLink = Array.from(article.querySelectorAll('a[href]')).find(
        (anchor) => isTargetStatusLink(anchor, statusId) && anchor.querySelector('time')
      );
      if (matchingTimeLink) return { article, timeLink: matchingTimeLink };
    }
    return null;
  }

  function serializeInlineNode(node, baseUrl) {
    if (node.nodeType === 3) return escapeHtml(node.nodeValue || '');
    if (node.nodeType !== 1) return '';

    const tagName = node.tagName.toLowerCase();
    if (tagName === 'br') return '<br>';
    if (tagName === 'img') return escapeHtml(node.getAttribute('alt') || '');

    const children = Array.from(node.childNodes)
      .map((child) => serializeInlineNode(child, baseUrl))
      .join('');

    if (tagName !== 'a') return children;

    const href = safeHttpUrl(node.getAttribute('href'), baseUrl);
    if (!href) return children;

    const label = children.trim() || escapeHtml(node.getAttribute('aria-label') || href);
    return `<a href="${escapeHtml(href)}">${label}</a>`;
  }

  function serializeTweetText(element, baseUrl) {
    if (!element) return '';
    const html = Array.from(element.childNodes)
      .map((node) => serializeInlineNode(node, baseUrl))
      .join('')
      .trim();
    if (html) return html;

    return escapeHtml(normalizedText(element)).replace(/\n/g, '<br>');
  }

  function findAuthor(article, username) {
    const handle = `@${username}`;
    const userNameBlock = article.querySelector('[data-testid="User-Name"]');
    if (!userNameBlock) return { displayName: username, handle };

    const expectedPath = `/${username.toLowerCase()}`;
    const candidateNames = Array.from(userNameBlock.querySelectorAll('a[href]'))
      .filter((anchor) => {
        try {
          return new URL(anchor.href, 'https://x.com').pathname.toLowerCase() === expectedPath;
        } catch (_error) {
          return false;
        }
      })
      .map((anchor) => stripVerifiedLabel(normalizedText(anchor)))
      .filter((text) => text && !text.startsWith('@'));

    return {
      displayName: candidateNames[0] || username,
      handle,
    };
  }

  function largerXImageUrl(value) {
    const normalizedUrl = safeHttpUrl(value);
    if (!normalizedUrl) return null;

    const parsed = new URL(normalizedUrl);
    if (parsed.hostname === 'pbs.twimg.com' && parsed.searchParams.has('name')) {
      parsed.searchParams.set('name', 'large');
    }
    return parsed.href;
  }

  function collectMedia(article, sourceUrl) {
    const images = [];
    const seenUrls = new Set();

    for (const image of article.querySelectorAll('[data-testid="tweetPhoto"] img')) {
      const src = largerXImageUrl(image.currentSrc || image.src || image.getAttribute('src'));
      if (!src || seenUrls.has(src)) continue;
      seenUrls.add(src);
      images.push({ src, alt: image.getAttribute('alt') || 'Image attached to this post' });
    }

    let videoPoster = null;
    if (article.querySelector('video')) {
      videoPoster = largerXImageUrl(article.querySelector('video').getAttribute('poster'));
    }

    return {
      images,
      videoPoster,
      hasVideo: Boolean(article.querySelector('video')),
      sourceUrl,
    };
  }

  function collectExternalLinks(article, baseUrl) {
    const links = [];
    const seenUrls = new Set();

    for (const anchor of article.querySelectorAll('a[href]')) {
      const href = safeHttpUrl(anchor.getAttribute('href'), baseUrl);
      if (!href) continue;

      const parsed = new URL(href);
      if (X_HOSTS.has(parsed.hostname.toLowerCase())) continue;
      if (parsed.hostname === 'pbs.twimg.com' || parsed.hostname === 'abs.twimg.com') continue;
      if (seenUrls.has(href)) continue;

      const label = normalizedText(anchor).replace(/\n+/g, ' ').trim();
      if (!label) continue;

      seenUrls.add(href);
      links.push({ href, label });
    }

    return links;
  }

  function makePostTitle(displayName, postText) {
    const excerpt = postText
      .replace(/\s+/g, ' ')
      .replace(/^@\S+\s*/, '')
      .trim();
    if (!excerpt) return `${displayName} on X`;

    const shortened = excerpt.length > MAX_TITLE_EXCERPT_LENGTH
      ? `${excerpt.slice(0, MAX_TITLE_EXCERPT_LENGTH - 1).trimEnd()}...`
      : excerpt;
    return `${displayName} on X: ${shortened}`;
  }

  function renderXPost(record) {
    const title = record.title || makePostTitle(record.displayName, record.postText);
    const timestamp = record.timestampLabel
      ? `<time${record.timestampIso ? ` datetime="${escapeHtml(record.timestampIso)}"` : ''}>${escapeHtml(record.timestampLabel)}</time>`
      : '';
    const authorMeta = [escapeHtml(record.handle), timestamp].filter(Boolean).join(' &middot; ');

    const imagesHtml = record.media.images
      .map((image) => `<figure><img class="x-post-media" src="${escapeHtml(image.src)}" alt="${escapeHtml(image.alt)}"></figure>`)
      .join('');

    const videoHtml = record.media.hasVideo
      ? `<figure class="x-post-video">${record.media.videoPoster ? `<img class="x-post-media" src="${escapeHtml(record.media.videoPoster)}" alt="Video preview">` : ''}<figcaption>Video attached. <a href="${escapeHtml(record.sourceUrl)}">Watch it on X</a>.</figcaption></figure>`
      : '';

    const linksHtml = record.externalLinks.length
      ? `<section class="x-post-links"><h2>Links</h2><ul>${record.externalLinks.map((link) => `<li><a href="${escapeHtml(link.href)}">${escapeHtml(link.label)}</a></li>`).join('')}</ul></section>`
      : '';

    return {
      title,
      author: record.handle,
      contentKind: 'x-post',
      content: `<article class="x-post"><header><h1>${escapeHtml(record.displayName)}</h1>${authorMeta ? `<p class="x-post-meta">${authorMeta}</p>` : ''}</header><div class="x-post-text">${record.postHtml}</div>${imagesHtml}${videoHtml}${linksHtml}<footer><a href="${escapeHtml(record.sourceUrl)}">Original post on X</a></footer></article>`,
    };
  }

  function extractXPost(doc, pageUrl) {
    const status = parseXStatusUrl(pageUrl);
    if (!status) throw new Error('This is not an X status URL.');

    const match = findTargetArticle(doc, status.statusId);
    if (!match) throw new Error('The target X post has not finished loading.');

    const { article, timeLink } = match;
    const textElement = article.querySelector('[data-testid="tweetText"]');
    const postText = normalizedText(textElement);
    const media = collectMedia(article, status.canonicalUrl);
    if (!postText && media.images.length === 0 && !media.hasVideo) {
      throw new Error('The target X post contains no readable text or media.');
    }

    const author = findAuthor(article, status.username);
    const timeElement = timeLink.querySelector('time');
    const record = {
      ...author,
      title: makePostTitle(author.displayName, postText),
      postText,
      postHtml: serializeTweetText(textElement, status.canonicalUrl) || '<p>Media post</p>',
      timestampLabel: normalizedText(timeElement),
      timestampIso: timeElement?.getAttribute('datetime') || '',
      media,
      externalLinks: collectExternalLinks(article, status.canonicalUrl),
      sourceUrl: status.canonicalUrl,
    };

    return renderXPost(record);
  }

  return {
    extractXPost,
    makePostTitle,
    parseXStatusUrl,
    renderXPost,
    safeHttpUrl,
  };
});
