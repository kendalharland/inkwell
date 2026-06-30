# Reading

inkwell's reader is built around three listing views, a single article
view, and a saved-for-later list. Every page is plain HTML — it works
identically on a Kindle's built-in browser and on a desktop.

## Listings

Three top-level listings share the same row format:

- **All stories** (`/`) merges every subscribed feed into one
  reverse-chronological stream.
- **Feeds** (`/feeds`) lists each individual feed; tapping one drills
  into that feed's stream.
- **Groups** (`/groups`) lists the groups defined in the admin page;
  tapping one drills into a merged stream for just that group.

Each row shows the article title, the source host, and a bookmark
icon. Tapping the title opens the article in the reader view; tapping
the bookmark icon saves or unsaves the article without leaving the
page.

### Pagination

Listings paginate at twenty entries per page. `Previous` and `Next`
links appear at the bottom of the page when more than one page is
available. The current page number is preserved when an article is
opened and again when it is closed, so reading and returning doesn't
lose the user's place.

## Article view

Tapping a listing entry loads the extracted article at
`/item/{id}`. inkwell pre-extracts every cached article during the
background refresh, so this page is typically a local SQLite read and
renders in a few hundred milliseconds even on a Kindle.

Each article page includes:

- The article title, with the same bookmark icon used on the listing.
- The article body, with images optimized for the Kindle browser (see
  [Image handling](#image-handling) below).
- A `Back` link returning to the listing page the article was opened
  from, including the page number.

### Image handling

Every `<img>` in a rendered article is rewritten to flow through
inkwell's server-side image proxy. The proxy fetches the source,
decodes it (JPEG, PNG, WebP, GIF), downscales anything larger than
1200&times;1600 pixels, flattens transparent pixels onto a solid
background, and re-encodes as JPEG under a 150&nbsp;KiB size budget.
The result is cached in the same SQLite file used for the article
cache so subsequent views read from local disk.

A short Cache-Control header lets the Kindle browser hold the image
in its own cache as well. Images are tagged with
`style="max-width:100%; height:auto"` so the Kindle scales them to
the column width regardless of the source dimensions.

If the proxy can't fetch or decode an image (the source returns 404,
the host is unreachable, the format is unsupported, etc.), it returns
404 and the Kindle renders the image's alt text in place. Images
referenced via `data:`, `file:`, or other non-HTTP schemes never hit
the proxy; they fall back to a styled alt-text block in the article
body.

The image cache ages out alongside articles — entries older than
`scheduler.article_ttl_days` are dropped by the same purge job that
removes stale articles.

### When extraction fails

Some sites refuse to serve their HTML to anything they identify as a
non-browser request (paywalls, JavaScript challenges, residual
bot-detection). inkwell sends a recent Chrome User-Agent and the same
`Accept` and `Accept-Language` headers a real browser would, which
clears most heuristics. Sites layered behind a JS challenge or an IP-
or TLS-fingerprint gate still return 4XX.

When that happens, the article page renders a short notice in place
of the article body and a direct link to the original URL. Tapping
the link opens the article in the Kindle's built-in browser, which
performs a fresh request from the device's IP and often succeeds
where the server-side extractor doesn't. Failed extractions are not
cached; tapping the same article later retries from scratch.

### Non-HTML articles

When a feed entry points to a non-HTML resource — a PDF, an image, a
video, or another binary file — inkwell does not try to extract it.
The article page instead renders a short notice and a direct link to
the original URL, so the device's built-in viewer (the Kindle PDF
reader, for example) can open it natively.

Detection uses the response's `Content-Type` header first and falls
back to the URL's path extension (e.g. `.pdf`), so links to PDFs that
servers return with a generic content type still take this path.

## Read later (bookmarks)

The bookmark icon next to every article toggles the saved-for-later
state. Saved articles appear at `/read-later`, accessible from the top
nav.

A bookmark stores the title and URL alongside the article id, so a
saved article stays visible in `/read-later` even after its source
feed has rolled the entry off and the article cache has purged the
body. Tapping a bookmarked article re-extracts it on demand if the
cache no longer holds a copy.

Bookmarked articles are never deleted by the purge job. Removing the
bookmark (tapping the filled bookmark icon) makes the article eligible
for purge again on the next sweep.

### Toggling without losing your place

Tapping the bookmark icon submits a form that round-trips to the
server and reloads the listing. To keep the page from snapping back to
the top after every toggle, inkwell redirects to the same URL with a
`#item-{id}` fragment appended, and every row in the listing carries a
matching anchor id. The browser scrolls back to the row that was
toggled, so a long page reads naturally even when many bookmarks are
flipped in a row. No JavaScript is involved — the redirect-and-anchor
trick works identically on every Kindle generation.

## Density and theme

Two top-nav links toggle layout preferences:

- **Density** switches between the default and a tighter compact
  layout that fits more rows per screen on small e-ink devices.
- **Theme** switches between light and dark.

Both preferences are sticky per browser via a cookie and survive
restarts. The initial state for a brand-new visitor is set by
`view.compact_default` and `view.dark_default` in the configuration
file. A single request can also override either preference without
touching the cookie by appending `?compact=1` / `?compact=0` or
`?theme=dark` / `?theme=light` to any listing URL.
