# Reading

The reader is served as static HTML, with no JavaScript. Every page
works identically on a Kindle's built-in browser and on a desktop.

## Views

| View        | Path             | Contents                                                            |
| ----------- | ---------------- | ------------------------------------------------------------------- |
| All stories | `/`              | Every subscribed feed merged, newest first.                         |
| Feeds       | `/feeds`         | One feed per row; tap a feed to see just its stream.                |
| Groups      | `/groups`        | One group per row; tap a group to see its feeds merged.             |
| Article     | `/item/{id}`     | The extracted article for one entry.                                |
| Read later  | `/read-later`    | Every bookmarked article, newest bookmark first.                    |

Every listing row shows the title, the source host, and a bookmark
icon. Tapping the title opens the article; tapping the icon toggles
the bookmark.

Listings paginate at twenty entries per page. `Previous` and `Next`
links appear at the bottom when there is more than one page.

## Bookmarks

The bookmark icon next to every article toggles the "read later"
state. Toggling it does not change the page: the row's icon flips
from outlined to filled (or vice versa) and the browser scrolls back
to that row.

`/read-later` lists every bookmarked article. A bookmark keeps its
title and URL alongside the article id, so a saved article stays
visible even after its source feed rolls the entry off. Tapping a
bookmarked article whose body has been purged re-extracts it on
demand.

Bookmarked articles are exempt from the purge job. Removing the
bookmark makes the article eligible for purge on the next sweep.

## Images

Every image in a rendered article is downscaled to fit within
1200×1600 pixels, flattened to a solid background, and re-encoded as
JPEG at ≤150 KiB. Results are cached; the same image renders from
local disk on subsequent views.

Images the transcoder cannot fetch or decode are replaced with the
image's `alt` text.

## Non-HTML articles

When a feed entry points to a PDF, an image, a video, or another
binary file, the article page renders a short notice and a link to
the original URL. The Kindle's built-in viewer opens the link
natively.

Detection uses the response's `Content-Type` header, falling back to
the URL's path extension (`.pdf`, etc.).

## When extraction fails

Some sites refuse HTML to server-side requests, either through paywall
gates, JavaScript challenges, or IP-based bot detection. When the
extractor cannot fetch the article, the article page renders a short
notice and a link to the original. Tapping the link opens the article
in the Kindle's browser, which fetches from the device's IP and often
succeeds where the server-side extractor does not.

Failed extractions are not cached; a later tap retries from scratch.

## Density and theme

Two links in the top nav toggle layout preferences:

- **Density** — default or compact (more rows per screen).
- **Theme** — light or dark.

Both are sticky per browser via a cookie. The initial state for a
new visitor comes from `view.compact_default` and `view.dark_default`
in the configuration file.

For a one-shot override without touching the cookie, append a query
parameter:

```
/?compact=1     # compact for this request
/?compact=0     # comfortable for this request
/?theme=dark    # dark for this request
/?theme=light   # light for this request
```
