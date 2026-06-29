# Admin

The `/admin` page is where the subscribed feeds and groups are managed
at runtime. Changes take effect immediately — no restart is required.

The configuration file's `rss:` block only seeds the database on the
first launch. Once any feed or group has been edited through the admin
page, the seed is ignored on subsequent restarts; the database is the
source of truth.

## Groups

Every feed belongs to at least one group. The Groups view in the top
nav lists every group, and tapping one drills into a merged stream of
just that group's feeds.

### Add a group

The **Add a group** form takes a group name and creates an empty
group. Group names are unique; submitting an existing name returns an
error.

### Remove a group

Each group in the admin list has a **Remove group** button. Confirming
the prompt deletes the group along with its feed subscriptions. The
articles those feeds had cached are not touched — they stay in the
read-later list and continue to appear under any other group still
subscribed to the same feed.

## Feeds

### Add a feed

Each group has an **Add feed** form. Typing into the input shows
autocomplete suggestions from any feed-search providers configured in
`feed_search.providers`; the default provider (`link_auto_discovery`)
takes a site URL and returns any RSS, Atom, or JSON-feed links
advertised in the page's `<link rel="alternate">` tags.

Tapping a suggestion fills the input with the feed URL. Submitting the
form subscribes the group to the feed.

Only `http://` and `https://` URLs are accepted. The same allow-list
also protects the article cache from saving bookmark URLs with unsafe
schemes.

### Remove a feed

Each feed row has a **Remove** button that drops the subscription
from that group only. If the same feed is subscribed under multiple
groups, only the chosen group's subscription is removed.

## Import OPML

The **Import OPML** form accepts an OPML 1.0 or 2.0 export from
another reader. Most readers (NetNewsWire, Feedly, Inoreader,
FreshRSS, …) export OPML directly; the file usually has an `.opml`
extension.

Selecting a file and submitting parses it, then merges its contents
into the database with the following rules:

- **Groups are merged by name.** A category in the OPML whose name
  matches an existing inkwell group adds its feeds to that group. New
  category names create new groups.
- **Feeds are deduplicated.** A feed URL already subscribed in the
  target group is skipped, not added twice. Re-importing the same OPML
  file is therefore a no-op.
- **Nested categories use the nearest label.** A feed under
  `<outline text="Tech"><outline text="Programming"><feed/></outline></outline>`
  lands in the `Programming` group, matching the user's visible
  hierarchy in the source reader.
- **Top-level (ungrouped) feeds** are placed in a group called
  `Uncategorized`. The group is created automatically if it doesn't
  exist yet.
- **Unsafe schemes are skipped.** Any `<outline xmlUrl>` whose URL
  isn't `http://` or `https://` is counted as skipped-invalid and not
  imported.

After import, a flash message at the top of the admin page reports
how many feeds were added, how many new groups were created, and how
many entries were skipped as duplicates or as invalid URLs.

The uploaded file is capped at 1 MiB.

## Restricting access

The admin page has no built-in authentication. When the server is
exposed beyond a trusted network, gate `/admin` at the reverse proxy
or behind an identity provider; see [self-hosting](self-hosting.md)
for examples.
